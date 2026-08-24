mod compact;
mod lifecycle;
mod regular;
mod review;
mod startup;
mod user_shell;

use std::sync::Arc;
use std::time::Instant;

use codex_diagnostics::Gauge;
use codex_extension_api::ThreadIdleCause;
use futures::future::BoxFuture;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;
use tracing::Span;
use tracing::field;
use tracing::info_span;
use tracing::trace;
use tracing::trace_span;
use tracing::warn;

use crate::codex_thread::BackgroundTerminalInfo;
use crate::codex_thread::TryStartTurnIfIdleRejectionReason;
use crate::config::Config;
use crate::context::ContextualUserFragment;
use crate::hook_runtime::run_turn_interrupt_hooks;
use crate::session::PendingInputFollowUp;
use crate::session::TurnInput;
use crate::session::classify_pending_input_follow_up;
use crate::session::session::Session;
use crate::session::turn::run_hooks_and_record_inputs;
use crate::session::turn_context::NewTurnContextOptions;
use crate::session::turn_context::TurnContext;
use crate::state::ActiveTurn;
use crate::state::RunningTask;
use crate::state::TaskKind;
use crate::state::TurnState;
use codex_analytics::TurnProfileFact;
use codex_analytics::TurnTokenUsageFact;
use codex_context_fragments::RenderedFragment;
use codex_otel::SessionTelemetry;
use codex_otel::TURN_E2E_DURATION_METRIC;
use codex_otel::TURN_MEMORY_METRIC;
use codex_otel::TURN_NETWORK_PROXY_METRIC;
use codex_otel::TURN_TOKEN_USAGE_METRIC;
use codex_otel::TURN_TOOL_CALL_METRIC;
use codex_otel::TURN_UNIFIED_EXEC_RUNNING_PROCESSES_METRIC;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::WarningEvent;
use codex_thread_store::PersistContext;

use codex_features::Feature;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
pub(crate) use compact::CompactTask;
pub(crate) use regular::RegularTask;
pub(crate) use review::ReviewTask;
use startup::TaskStartupCompletionGuard;
pub(crate) use startup::TaskStartupState;
use startup::wait_for_task_completion_with_grace;
pub(crate) use user_shell::UserShellCommandPlacement;
pub(crate) use user_shell::execute_user_shell_command;

pub(crate) const GRACEFULL_INTERRUPTION_TIMEOUT_MS: u64 = 100;
const TASK_COMPACT_METRIC: &str = "codex.task.compact";
static ACTIVE_TURNS: Gauge = Gauge::new("core.turns.active");

pub(crate) type SessionTaskResult = CodexResult<Option<String>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TaskStartupOutcome {
    Run,
    Finish,
}

/// Whether task finalization completed or reserved the active task for newly accepted input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TaskFinishAction {
    Finish,
    Continue,
}

pub(crate) enum MailboxParentProvenance {
    Ignore,
    Attribute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterruptedTurnHistoryMarker {
    Disabled,
    ContextualUser,
    Developer,
}

impl InterruptedTurnHistoryMarker {
    pub(crate) fn from_config_and_version(
        config: &Config,
        multi_agent_version: MultiAgentVersion,
    ) -> Self {
        if !config.agent_interrupt_message_enabled {
            return Self::Disabled;
        }
        if multi_agent_version == MultiAgentVersion::V2 {
            Self::Developer
        } else {
            Self::ContextualUser
        }
    }
}

/// Shared model-visible marker used by both the real interrupt path and
/// interrupted fork snapshots.
pub(crate) fn interrupted_turn_history_marker(
    marker: InterruptedTurnHistoryMarker,
) -> Option<ResponseItem> {
    match marker {
        InterruptedTurnHistoryMarker::Disabled => None,
        InterruptedTurnHistoryMarker::ContextualUser => Some(ContextualUserFragment::into(
            crate::context::TurnAborted::new(crate::context::TurnAborted::INTERRUPTED_GUIDANCE),
        )),
        InterruptedTurnHistoryMarker::Developer => {
            let marker = crate::context::TurnAborted::new(
                crate::context::TurnAborted::INTERRUPTED_DEVELOPER_GUIDANCE,
            );
            let (_, content) = marker.render_fragment().into_parts();
            Some(RenderedFragment::new("developer", content).into())
        }
    }
}

fn emit_turn_network_proxy_metric(
    session_telemetry: &SessionTelemetry,
    network_proxy_active: bool,
    tmp_mem: (&str, &str),
) {
    let active = if network_proxy_active {
        "true"
    } else {
        "false"
    };
    session_telemetry.counter(
        TURN_NETWORK_PROXY_METRIC,
        /*inc*/ 1,
        &[("active", active), tmp_mem],
    );
}

fn emit_turn_memory_metric(
    session_telemetry: &SessionTelemetry,
    feature_enabled: bool,
    config_enabled: bool,
    has_citations: bool,
) {
    let read_allowed = feature_enabled && config_enabled;
    session_telemetry.counter(
        TURN_MEMORY_METRIC,
        /*inc*/ 1,
        &[
            ("read_allowed", bool_tag(read_allowed)),
            ("feature_enabled", bool_tag(feature_enabled)),
            ("config_use_memories", bool_tag(config_enabled)),
            ("has_citations", bool_tag(has_citations)),
        ],
    );
}

pub(crate) fn emit_compact_metric(
    session_telemetry: &SessionTelemetry,
    compact_type: &'static str,
    manual: bool,
) {
    session_telemetry.counter(
        TASK_COMPACT_METRIC,
        /*inc*/ 1,
        &[("type", compact_type), ("manual", bool_tag(manual))],
    );
}

fn bool_tag(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

/// Async task that drives a [`Session`] turn.
///
/// Implementations encapsulate a specific Codex workflow (regular chat,
/// reviews, ghost snapshots, etc.). Each task instance is owned by a
/// [`Session`] and executed on a background Tokio task. The trait is
/// intentionally small: implementers identify themselves via
/// [`SessionTask::kind`], perform their work in [`SessionTask::run`], and may
/// release resources in [`SessionTask::abort`].
pub(crate) trait SessionTask: Send + Sync + 'static {
    /// Describes the type of work the task performs so the session can
    /// surface it in telemetry and UI.
    fn kind(&self) -> TaskKind;

    /// Returns the tracing name for a spawned task span.
    fn span_name(&self) -> &'static str;

    /// Completes lifecycle work that a forced abort must not discard.
    ///
    /// Core tracks this phase separately from ordinary task work. Once cancellation starts, Core
    /// still allows startup to finish before applying the normal forced-abort timeout again.
    /// Return [`TaskStartupOutcome::Finish`] after startup has handled the cancelled task's input.
    fn run_startup<'a>(
        &'a self,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        input: &'a [TurnInput],
        cancellation_token: &'a CancellationToken,
    ) -> impl std::future::Future<Output = TaskStartupOutcome> + Send + 'a {
        async move {
            let _ = (self, session, ctx, input, cancellation_token);
            TaskStartupOutcome::Run
        }
    }

    /// Executes the task until completion or cancellation.
    ///
    /// Implementations typically stream protocol events using `session` and
    /// `ctx`, returning an optional final agent message when finished. The
    /// provided `cancellation_token` is cancelled when the session requests an
    /// abort; implementers should watch for it and terminate quickly once it
    /// fires. Returning [`Some`] yields a final message that
    /// [`Session::on_task_finished`] will emit to the client. Returning
    /// [`CodexErr::TurnAborted`] completes the task through the aborted-turn
    /// lifecycle instead.
    fn run(
        self: Arc<Self>,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = SessionTaskResult> + Send;

    /// Whether the task can continue when input is accepted after its last pending-input check.
    ///
    /// Implementations returning `true` must override
    /// [`SessionTask::run_pending_input_continuation`].
    fn supports_pending_input_continuation(&self) -> bool {
        false
    }

    /// Continues the existing turn without repeating its turn-start lifecycle.
    fn run_pending_input_continuation(
        self: Arc<Self>,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = SessionTaskResult> + Send {
        async move {
            let _ = (self, session, ctx, cancellation_token);
            unreachable!("task does not support pending input continuation")
        }
    }

    /// Gives the task a chance to perform cleanup after an abort.
    ///
    /// The default implementation is a no-op; override this if additional
    /// teardown or notifications are required once
    /// [`Session::abort_all_tasks`] cancels the task.
    fn abort(
        &self,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
    ) -> impl std::future::Future<Output = ()> + Send {
        async move {
            let _ = (session, ctx);
        }
    }
}

pub(crate) trait AnySessionTask: Send + Sync + 'static {
    fn kind(&self) -> TaskKind;

    fn span_name(&self) -> &'static str;

    fn run_startup<'a>(
        &'a self,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        input: &'a [TurnInput],
        cancellation_token: &'a CancellationToken,
    ) -> BoxFuture<'a, TaskStartupOutcome>;

    fn run(
        self: Arc<Self>,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> BoxFuture<'static, SessionTaskResult>;

    fn supports_pending_input_continuation(&self) -> bool;

    fn run_pending_input_continuation(
        self: Arc<Self>,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        cancellation_token: CancellationToken,
    ) -> BoxFuture<'static, SessionTaskResult>;

    fn abort<'a>(&'a self, session: Arc<Session>, ctx: Arc<TurnContext>) -> BoxFuture<'a, ()>;
}

impl<T> AnySessionTask for T
where
    T: SessionTask,
{
    fn kind(&self) -> TaskKind {
        SessionTask::kind(self)
    }

    fn span_name(&self) -> &'static str {
        SessionTask::span_name(self)
    }

    fn run_startup<'a>(
        &'a self,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        input: &'a [TurnInput],
        cancellation_token: &'a CancellationToken,
    ) -> BoxFuture<'a, TaskStartupOutcome> {
        Box::pin(SessionTask::run_startup(
            self,
            session,
            ctx,
            input,
            cancellation_token,
        ))
    }

    fn run(
        self: Arc<Self>,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> BoxFuture<'static, SessionTaskResult> {
        Box::pin(SessionTask::run(
            self,
            session,
            ctx,
            input,
            cancellation_token,
        ))
    }

    fn supports_pending_input_continuation(&self) -> bool {
        <T as SessionTask>::supports_pending_input_continuation(self)
    }

    fn run_pending_input_continuation(
        self: Arc<Self>,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        cancellation_token: CancellationToken,
    ) -> BoxFuture<'static, SessionTaskResult> {
        Box::pin(<T as SessionTask>::run_pending_input_continuation(
            self,
            session,
            ctx,
            cancellation_token,
        ))
    }

    fn abort<'a>(&'a self, session: Arc<Session>, ctx: Arc<TurnContext>) -> BoxFuture<'a, ()> {
        Box::pin(SessionTask::abort(self, session, ctx))
    }
}

impl Session {
    pub async fn spawn_task<T: SessionTask>(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        input: Vec<TurnInput>,
        task: T,
    ) {
        self.abort_all_tasks(TurnAbortReason::Replaced).await;
        self.clear_connector_selection().await;
        self.start_task(
            turn_context,
            input,
            task,
            /*input_persisted*/ None,
            MailboxParentProvenance::Ignore,
        )
        .await;
    }

    pub(crate) async fn start_task<T: SessionTask>(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        input: Vec<TurnInput>,
        task: T,
        input_persisted: Option<
            tokio::sync::oneshot::Sender<Result<(), TryStartTurnIfIdleRejectionReason>>,
        >,
        mailbox_parent_provenance: MailboxParentProvenance,
    ) {
        let task: Arc<dyn AnySessionTask> = Arc::new(task);
        let task_kind = task.kind();
        let span_name = task.span_name();
        if !self.begin_agent_response_turn(&turn_context.sub_id) {
            return;
        }
        let started_at = Instant::now();
        let turn_started_at_unix_ms = turn_context
            .turn_timing_state
            .mark_turn_started(started_at)
            .await;
        turn_context
            .turn_metadata_state
            .set_turn_started_at_unix_ms(turn_started_at_unix_ms);
        let token_usage_at_turn_start = self.total_token_usage().await.unwrap_or_default();

        let cancellation_token = CancellationToken::new();
        let done = Arc::new(Notify::new());

        self.services
            .guardian_rejection_circuit_breaker
            .lock()
            .await
            .clear_turn(&turn_context.sub_id);

        let queued_items = self.input_queue.take_queued_items_for_next_turn().await;
        let (mailbox_items, start_options) =
            self.input_queue.get_pending_input(&self.active_turn).await;
        if let MailboxParentProvenance::Attribute = mailbox_parent_provenance {
            if let Some(id) = start_options.parent_turn_id {
                if let Some(initiating_agent_path) = mailbox_items.iter().find_map(|item| {
                    let TurnInput::InterAgentCommunication(communication) = item else {
                        return None;
                    };
                    communication
                        .trigger_turn
                        .then(|| communication.author.clone())
                }) {
                    turn_context
                        .turn_metadata_state
                        .set_initiating_agent_path(initiating_agent_path);
                }
                turn_context.turn_metadata_state.set_parent_turn_id(id);
            }
            if let Some(id) = start_options.root_turn_id {
                turn_context.turn_metadata_state.set_root_turn_id(id);
            }
        } else if turn_context.turn_metadata_state.root_turn_id().is_none()
            && let Some(root_turn_id) = start_options.root_turn_id
        {
            turn_context
                .turn_metadata_state
                .set_root_turn_id(root_turn_id);
        }
        let turn_state = {
            let mut active = self.active_turn.lock().await;
            let turn = active.get_or_insert_with(ActiveTurn::default);
            debug_assert!(turn.task.is_none());
            Arc::clone(&turn.turn_state)
        };
        turn_state.lock().await.token_usage_at_turn_start = token_usage_at_turn_start.clone();
        let mut task_input = input;
        let mut pending_items = queued_items;
        if let Some(prompt_index) = task_input.iter().position(TurnInput::is_prompt) {
            task_input.splice(prompt_index..prompt_index, mailbox_items);
        } else {
            pending_items.extend(mailbox_items);
        }
        self.input_queue
            .extend_pending_input_for_turn_state(turn_state.as_ref(), pending_items)
            .await;
        self.emit_turn_start_lifecycle(turn_context.as_ref(), &token_usage_at_turn_start)
            .await;

        let mut active = self.active_turn.lock().await;
        let turn = active.get_or_insert_with(ActiveTurn::default);
        debug_assert!(turn.task.is_none());
        let agent_execution_guard = self.services.agent_control.execution_guard(
            turn_context.multi_agent_version,
            &turn_context.session_source,
        );
        let done_clone = Arc::clone(&done);
        let startup = Arc::new(TaskStartupState::default());
        let startup_for_run = Arc::clone(&startup);
        let session = Arc::clone(self);
        let ctx = Arc::clone(&turn_context);
        let task_for_run = Arc::clone(&task);
        let task_cancellation_token = cancellation_token.child_token();
        // Task-owned turn spans keep a core-owned span open for the
        // full task lifecycle after the submission dispatch span ends.
        let reasoning_effort = turn_context.effective_reasoning_effort_for_tracing();
        let task_span = info_span!(
            "turn",
            otel.name = span_name,
            thread.id = %self.thread_id,
            turn.id = %turn_context.sub_id,
            model = %turn_context.model_info().slug,
            codex.turn.reasoning_effort = %reasoning_effort,
            codex.turn.token_usage.input_tokens = field::Empty,
            codex.turn.token_usage.cached_input_tokens = field::Empty,
            codex.turn.token_usage.cache_write_input_tokens = field::Empty,
            codex.turn.token_usage.non_cached_input_tokens = field::Empty,
            codex.turn.token_usage.output_tokens = field::Empty,
            codex.turn.token_usage.reasoning_output_tokens = field::Empty,
            codex.turn.token_usage.total_tokens = field::Empty,
        );
        let handle = tokio::spawn(
            async move {
                let startup_guard =
                    TaskStartupCompletionGuard::new(Arc::clone(&startup_for_run));
                let startup_outcome = task_for_run
                    .run_startup(
                        Arc::clone(&session),
                        Arc::clone(&ctx),
                        &task_input,
                        &task_cancellation_token,
                    )
                    .instrument(trace_span!("session_task.run_startup"))
                    .await;
                drop(startup_guard);
                let ctx_for_finish = Arc::clone(&ctx);
                let mut task_result = match startup_outcome {
                    TaskStartupOutcome::Run => {
                        Arc::clone(&task_for_run)
                            .run(
                                Arc::clone(&session),
                                ctx,
                                task_input,
                                task_cancellation_token.child_token(),
                            )
                            .instrument(trace_span!("session_task.run"))
                            .await
                    }
                    TaskStartupOutcome::Finish => Ok(None),
                };
                let sess = Arc::clone(&session);
                let mut last_agent_message = None;
                loop {
                    // Match run_turn's handling of late follow-up work: an acknowledgement with
                    // no visible final text must not erase the turn's last visible answer.
                    if let Ok(Some(message)) = &task_result {
                        last_agent_message = Some(message.clone());
                    } else if let (Ok(None), Some(message)) = (&task_result, &last_agent_message) {
                        task_result = Ok(Some(message.clone()));
                    }
                    if let Err(err) = sess.flush_rollout().await {
                        warn!("failed to flush rollout before completing turn: {err}");
                        sess.send_event(
                            ctx_for_finish.as_ref(),
                            EventMsg::Warning(WarningEvent {
                                message: format!(
                                    "Failed to save the conversation transcript; Codex will continue retrying. Error: {err}"
                                ),
                            }),
                        )
                        .await;
                    }
                    if task_cancellation_token.is_cancelled() {
                        break;
                    }
                    // Finish uniformly from the spawn site so all tasks share the same lifecycle.
                    match sess
                        .on_task_finished(Arc::clone(&ctx_for_finish), task_result)
                        .await
                    {
                        TaskFinishAction::Finish => break,
                        TaskFinishAction::Continue => {
                            task_result = Arc::clone(&task_for_run)
                                .run_pending_input_continuation(
                                    Arc::clone(&session),
                                    Arc::clone(&ctx_for_finish),
                                    task_cancellation_token.child_token(),
                                )
                                .instrument(trace_span!(
                                    "session_task.run_pending_input_continuation"
                                ))
                                .await;
                        }
                    }
                }
                done_clone.notify_waiters();
            }
            .instrument(task_span),
        );
        let timer = turn_context
            .session_telemetry
            .start_timer(TURN_E2E_DURATION_METRIC, &[])
            .ok();
        let running_task = RunningTask {
            done,
            startup,
            handle: AbortOnDropHandle::new(handle),
            kind: task_kind,
            task,
            cancellation_token,
            turn_context: Arc::clone(&turn_context),
            input_persisted,
            _agent_execution_guard: agent_execution_guard,
            _diagnostics_guard: ACTIVE_TURNS.track(),
            _timer: timer,
        };
        turn.task = Some(running_task);
        drop(active);
        self.active_turn_transition.notify_waiters();
    }

    /// Returns whether an extension has marked this thread as durably asleep.
    pub(crate) fn has_outstanding_durable_sleep(&self) -> bool {
        self.services
            .thread_extension_data
            .get::<codex_extension_items::sleep::SleepItem>()
            .is_some()
    }

    /// Starts a regular turn when the session is idle and pending work is waiting.
    ///
    /// Pending work includes inputs queued behind a context boundary, mailbox mail marked with
    /// `trigger_turn`, or any mailbox mail while an outstanding durable sleep is attached.
    ///
    /// This helper generates a fresh sub-id for the synthetic turn before delegating to the
    /// explicit-sub-id variant.
    pub(crate) fn maybe_start_turn_for_pending_work(self: &Arc<Self>) -> BoxFuture<'static, ()> {
        let session = Arc::clone(self);
        Box::pin(async move {
            session
                .maybe_start_turn_for_pending_work_with_sub_id(uuid::Uuid::new_v4().to_string())
                .await;
        })
    }

    /// Starts a regular turn with the provided sub-id when pending work should wake an idle
    /// session.
    ///
    /// The turn is created only when the session is idle and queued input is waiting, or mailbox
    /// mail either requests a turn or can wake an outstanding durable sleep.
    pub(crate) async fn maybe_start_turn_for_pending_work_with_sub_id(
        self: &Arc<Self>,
        sub_id: String,
    ) {
        // codex exec is a one-shot host: its client leaves when the primary turn completes.
        // Mail may still steer that active turn, but must not synthesize a later idle turn during
        // the narrow interval before exec consumes the primary TurnCompleted notification.
        if self
            .app_server_client_metadata()
            .await
            .client_name
            .as_deref()
            == Some("codex_exec")
        {
            return;
        }
        if !self.input_queue.has_queued_turn_inputs().await
            && (!self.input_queue.has_pending_mailbox_items().await
                || (!self.input_queue.has_trigger_turn_mailbox_items().await
                    && !self.has_outstanding_durable_sleep()))
        {
            return;
        }

        let turn_state = {
            let mut active_turn = self.active_turn.lock().await;
            if active_turn.is_some() {
                return;
            }
            let active_turn = active_turn.get_or_insert_with(ActiveTurn::default);
            Arc::clone(&active_turn.turn_state)
        };

        let (input, mut start_options) =
            self.input_queue.get_pending_input(&self.active_turn).await;
        if !input.iter().any(
            |item| matches!(item, TurnInput::InterAgentCommunication(mail) if mail.trigger_turn),
        ) {
            // Queue-only mail wakes durable sleep without selecting a new task's settings.
            start_options.cyber_access_program = self
                .reference_context_item()
                .await
                .and_then(|context| context.cyber_access_program);
        }
        let turn_context = self
            .new_turn_with_default_settings(
                sub_id,
                NewTurnContextOptions {
                    final_output_json_schema: start_options.final_output_json_schema,
                    cyber_access_program: start_options.cyber_access_program,
                },
            )
            .await;
        if let Some(id) = start_options.parent_turn_id {
            if let Some(initiating_agent_path) = input.iter().find_map(|item| {
                let TurnInput::InterAgentCommunication(communication) = item else {
                    return None;
                };
                communication
                    .trigger_turn
                    .then(|| communication.author.clone())
            }) {
                turn_context
                    .turn_metadata_state
                    .set_initiating_agent_path(initiating_agent_path);
            }
            turn_context.turn_metadata_state.set_parent_turn_id(id);
        }
        if let Some(id) = start_options.root_turn_id {
            turn_context.turn_metadata_state.set_root_turn_id(id);
        }
        self.maybe_emit_model_warnings_for_turn(turn_context.as_ref())
            .await;
        // Task completion must still save this mail if pre-turn compaction fails.
        self.input_queue
            .extend_pending_input_for_turn_state(turn_state.as_ref(), input)
            .await;
        self.start_task(
            turn_context,
            Vec::new(),
            RegularTask::new(),
            /*input_persisted*/ None,
            MailboxParentProvenance::Attribute,
        )
        .await;
    }

    pub async fn abort_all_tasks(self: &Arc<Self>, reason: TurnAbortReason) {
        let (aborted, cleared_taskless_turn) = {
            let mut active = self.active_turn.lock().await;
            if matches!(
                reason,
                TurnAbortReason::Interrupted | TurnAbortReason::BudgetLimited
            ) && active
                .as_ref()
                .is_some_and(|active_turn| active_turn.task.is_some())
            {
                self.mark_interrupted();
            }
            let aborted = active.as_mut().and_then(|active_turn| {
                let task = active_turn.task.take()?;
                let active_turn = ActiveTurn {
                    task: None,
                    turn_state: Arc::clone(&active_turn.turn_state),
                };
                Some((active_turn, task))
            });
            let cleared_taskless_turn = aborted.is_none()
                && active
                    .as_ref()
                    .is_some_and(|active_turn| active_turn.task.is_none());
            if cleared_taskless_turn {
                *active = None;
            }
            (aborted, cleared_taskless_turn)
        };
        if cleared_taskless_turn {
            self.active_turn_transition.notify_waiters();
        }
        let Some((active_turn, task)) = aborted else {
            return;
        };
        let turn_context = Arc::clone(&task.turn_context);
        let aborted_event = self
            .handle_task_abort(&active_turn, task, reason.clone())
            .await;
        self.emit_turn_abort_lifecycle(reason.clone(), turn_context.extension_data.as_ref())
            .await;
        // Let interrupted tasks observe cancellation before dropping pending approvals, or an
        // in-flight approval wait can surface as a model-visible rejection before TurnAborted.
        self.record_active_turn_mcp_server_use_context_before_abort(&active_turn)
            .await;
        self.input_queue.clear_pending(&active_turn).await;
        let cleared_active_turn = {
            let mut active = self.active_turn.lock().await;
            if active.as_ref().is_some_and(|current| {
                current.task.is_none() && Arc::ptr_eq(&current.turn_state, &active_turn.turn_state)
            }) {
                *active = None;
                true
            } else {
                false
            }
        };
        if cleared_active_turn {
            self.active_turn_transition.notify_waiters();
        }
        self.publish_task_abort_event(turn_context.as_ref(), aborted_event)
            .await;
        if reason == TurnAbortReason::Interrupted && cleared_active_turn {
            self.maybe_start_turn_for_pending_work().await;
        }
    }

    pub(crate) async fn abort_turn_if_active(
        self: &Arc<Self>,
        turn_id: &str,
        reason: TurnAbortReason,
    ) -> bool {
        let aborted = {
            let mut active = self.active_turn.lock().await;
            if matches!(
                reason,
                TurnAbortReason::Interrupted | TurnAbortReason::BudgetLimited
            ) && active
                .as_ref()
                .and_then(|active_turn| active_turn.task.as_ref())
                .is_some_and(|task| task.turn_context.sub_id == turn_id)
            {
                self.mark_interrupted();
            }
            active.as_mut().and_then(|active_turn| {
                let task = active_turn
                    .task
                    .as_ref()
                    .filter(|task| task.turn_context.sub_id == turn_id)?;
                let turn_context = Arc::clone(&task.turn_context);
                let task = active_turn.task.take()?;
                let active_turn = ActiveTurn {
                    task: None,
                    turn_state: Arc::clone(&active_turn.turn_state),
                };
                Some((active_turn, task, turn_context))
            })
        };
        let Some((active_turn, task, turn_context)) = aborted else {
            return false;
        };

        let aborted_event = self
            .handle_task_abort(&active_turn, task, reason.clone())
            .await;
        self.emit_turn_abort_lifecycle(reason.clone(), turn_context.extension_data.as_ref())
            .await;
        // Let interrupted tasks observe cancellation before dropping pending approvals, or an
        // in-flight approval wait can surface as a model-visible rejection before TurnAborted.
        self.record_active_turn_mcp_server_use_context_before_abort(&active_turn)
            .await;
        self.input_queue.clear_pending(&active_turn).await;
        let cleared_active_turn = {
            let mut active = self.active_turn.lock().await;
            if active.as_ref().is_some_and(|current| {
                current.task.is_none() && Arc::ptr_eq(&current.turn_state, &active_turn.turn_state)
            }) {
                *active = None;
                true
            } else {
                false
            }
        };
        if cleared_active_turn {
            self.active_turn_transition.notify_waiters();
        }
        self.publish_task_abort_event(turn_context.as_ref(), aborted_event)
            .await;

        if reason == TurnAbortReason::Interrupted && cleared_active_turn {
            self.maybe_start_turn_for_pending_work().await;
        }

        true
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "task removal and the final pending-input check must remain atomic with steering"
    )]
    pub async fn on_task_finished(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        task_result: SessionTaskResult,
    ) -> TaskFinishAction {
        let can_continue = task_result.is_ok();
        let (last_agent_message, abort_reason) = match task_result {
            Ok(last_agent_message) => (last_agent_message, None),
            Err(err) if matches!(err.details(), CodexErrorDetails::TurnAborted) => {
                (None, Some(TurnAbortReason::Interrupted))
            }
            Err(err) => {
                warn!(%err, "session task returned an unexpected error");
                self.emit_turn_error_lifecycle(
                    turn_context.as_ref(),
                    err.to_codex_protocol_error(),
                )
                .await;
                self.track_turn_codex_error(turn_context.as_ref(), &err);
                self.send_event(
                    turn_context.as_ref(),
                    EventMsg::Error(err.to_error_event(/*message_prefix*/ None)),
                )
                .await;
                (None, None)
            }
        };
        let turn_state = {
            let mut active = self.active_turn.lock().await;
            let Some(active_turn) = active.as_mut() else {
                return TaskFinishAction::Finish;
            };
            let Some(task) = active_turn.task.as_ref() else {
                return TaskFinishAction::Finish;
            };
            // Steering takes these locks in the same order. It therefore either publishes input
            // before this check and reserves a continuation, or observes the detached task and
            // starts/rejects work through the ordinary idle-turn path.
            let should_continue =
                can_continue && task.task.supports_pending_input_continuation() && {
                    let turn_state = active_turn.turn_state.lock().await;
                    matches!(
                        classify_pending_input_follow_up(&turn_state),
                        PendingInputFollowUp::Required
                    )
                };
            if should_continue {
                return TaskFinishAction::Continue;
            }
            let Some(mut task) = active_turn.task.take() else {
                return TaskFinishAction::Finish;
            };
            if let Some(sender) = task.input_persisted.take() {
                let _ = sender.send(Err(
                    TryStartTurnIfIdleRejectionReason::TaskEndedBeforePersistence,
                ));
            }
            task.handle.detach();
            Arc::clone(&active_turn.turn_state)
        };
        turn_context
            .turn_metadata_state
            .cancel_git_enrichment_task();
        let mut pending_input = self
            .input_queue
            .take_pending_input_for_turn_state(turn_state.as_ref())
            .await;
        let (turn_had_memory_citation, turn_tool_calls, token_usage_at_turn_start) = {
            let ts = turn_state.lock().await;
            (
                ts.has_memory_citation,
                ts.tool_calls,
                ts.token_usage_at_turn_start.clone(),
            )
        };
        let mut pending_input_after_mcp_use = Vec::new();
        if let Some(mcp_index) = pending_input
            .iter()
            .position(is_mcp_server_use_context_input)
            && let Some(first_non_mcp_after_boundary) = pending_input
                .iter()
                .enumerate()
                .skip(mcp_index)
                .find_map(|(index, item)| (!is_mcp_server_use_context_input(item)).then_some(index))
        {
            pending_input_after_mcp_use = pending_input.split_off(first_non_mcp_after_boundary);
        }
        run_hooks_and_record_inputs(
            self,
            &turn_context,
            &pending_input,
            PersistContext::Standard,
        )
        .await;
        let queued_pending_input_after_mcp_use = !pending_input_after_mcp_use.is_empty();
        self.input_queue
            .queue_turn_inputs_for_next_turn(pending_input_after_mcp_use)
            .await;
        // Emit token usage metrics.
        {
            // TODO(jif): drop this
            let tmp_mem = (
                "tmp_mem_enabled",
                if self.enabled(Feature::MemoryTool) {
                    "true"
                } else {
                    "false"
                },
            );
            let network_proxy = self.services.network_proxy.load_full();
            let network_proxy_active = match network_proxy.as_ref() {
                Some(started_network_proxy) => {
                    match started_network_proxy.proxy().current_cfg().await {
                        Ok(config) => config.enabled,
                        Err(err) => {
                            warn!(
                                "failed to read managed network proxy state for turn metrics: {err:#}"
                            );
                            false
                        }
                    }
                }
                None => false,
            };
            emit_turn_network_proxy_metric(
                &self.services.session_telemetry,
                network_proxy_active,
                tmp_mem,
            );
            self.services.session_telemetry.histogram(
                TURN_TOOL_CALL_METRIC,
                i64::try_from(turn_tool_calls).unwrap_or(i64::MAX),
                &[tmp_mem],
            );
            let total_token_usage = self.total_token_usage().await.unwrap_or_default();
            let turn_token_usage = TokenUsage {
                input_tokens: (total_token_usage.input_tokens
                    - token_usage_at_turn_start.input_tokens)
                    .max(0),
                cached_input_tokens: (total_token_usage.cached_input_tokens
                    - token_usage_at_turn_start.cached_input_tokens)
                    .max(0),
                cache_write_input_tokens: (total_token_usage.cache_write_input_tokens
                    - token_usage_at_turn_start.cache_write_input_tokens)
                    .max(0),
                output_tokens: (total_token_usage.output_tokens
                    - token_usage_at_turn_start.output_tokens)
                    .max(0),
                reasoning_output_tokens: (total_token_usage.reasoning_output_tokens
                    - token_usage_at_turn_start.reasoning_output_tokens)
                    .max(0),
                total_tokens: (total_token_usage.total_tokens
                    - token_usage_at_turn_start.total_tokens)
                    .max(0),
                codex_rollout_budget_units: None,
            };
            let current_span = Span::current();
            current_span.record(
                "codex.turn.token_usage.input_tokens",
                turn_token_usage.input_tokens,
            );
            current_span.record(
                "codex.turn.token_usage.cached_input_tokens",
                turn_token_usage.cached_input(),
            );
            current_span.record(
                "codex.turn.token_usage.cache_write_input_tokens",
                turn_token_usage.cache_write_input_tokens,
            );
            current_span.record(
                "codex.turn.token_usage.non_cached_input_tokens",
                turn_token_usage.non_cached_input(),
            );
            current_span.record(
                "codex.turn.token_usage.output_tokens",
                turn_token_usage.output_tokens,
            );
            current_span.record(
                "codex.turn.token_usage.reasoning_output_tokens",
                turn_token_usage.reasoning_output_tokens,
            );
            current_span.record(
                "codex.turn.token_usage.total_tokens",
                turn_token_usage.total_tokens,
            );
            self.services
                .analytics_events_client
                .track_turn_token_usage(TurnTokenUsageFact {
                    turn_id: turn_context.sub_id.clone(),
                    thread_id: self.thread_id.to_string(),
                    token_usage: turn_token_usage.clone(),
                });
            self.services.session_telemetry.histogram(
                TURN_TOKEN_USAGE_METRIC,
                turn_token_usage.total_tokens,
                &[("token_type", "total"), tmp_mem],
            );
            self.services.session_telemetry.histogram(
                TURN_TOKEN_USAGE_METRIC,
                turn_token_usage.input_tokens,
                &[("token_type", "input"), tmp_mem],
            );
            self.services.session_telemetry.histogram(
                TURN_TOKEN_USAGE_METRIC,
                turn_token_usage.cached_input(),
                &[("token_type", "cached_input"), tmp_mem],
            );
            self.services.session_telemetry.histogram(
                TURN_TOKEN_USAGE_METRIC,
                turn_token_usage.cache_write_input_tokens,
                &[("token_type", "cache_write_input"), tmp_mem],
            );
            self.services.session_telemetry.histogram(
                TURN_TOKEN_USAGE_METRIC,
                turn_token_usage.output_tokens,
                &[("token_type", "output"), tmp_mem],
            );
            self.services.session_telemetry.histogram(
                TURN_TOKEN_USAGE_METRIC,
                turn_token_usage.reasoning_output_tokens,
                &[("token_type", "reasoning_output"), tmp_mem],
            );
        }
        emit_turn_memory_metric(
            &self.services.session_telemetry,
            turn_context.config.features.enabled(Feature::MemoryTool),
            turn_context.config.memories.use_memories,
            turn_had_memory_citation,
        );
        self.services.session_telemetry.counter(
            TURN_UNIFIED_EXEC_RUNNING_PROCESSES_METRIC,
            i64::try_from(self.list_background_terminals().await.len()).unwrap_or(i64::MAX),
            &[],
        );
        let started_at = turn_context.turn_timing_state.started_at_unix_secs().await;
        let (completed_at, duration_ms, profile) = turn_context
            .turn_timing_state
            .complete_profile_and_duration_ms()
            .await;
        self.services
            .analytics_events_client
            .track_turn_profile(TurnProfileFact {
                turn_id: turn_context.sub_id.clone(),
                profile,
            });
        let idle_cause = if matches!(
            abort_reason.as_ref(),
            Some(TurnAbortReason::Interrupted | TurnAbortReason::BudgetLimited)
        ) {
            ThreadIdleCause::Interrupted
        } else if abort_reason.is_none() && turn_context.terminal_error.lock().await.is_some() {
            ThreadIdleCause::Failed
        } else {
            ThreadIdleCause::Completed
        };
        let event = if let Some(reason) = abort_reason {
            if reason == TurnAbortReason::Interrupted {
                run_turn_interrupt_hooks(self, &turn_context, &turn_state).await;
            }
            self.emit_turn_abort_lifecycle(reason.clone(), turn_context.extension_data.as_ref())
                .await;
            EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some(turn_context.sub_id.clone()),
                reason,
                started_at,
                completed_at,
                duration_ms,
            })
        } else {
            let time_to_first_token_ms = turn_context
                .turn_timing_state
                .time_to_first_token_ms()
                .await;
            let error = turn_context.terminal_error.lock().await.clone();
            self.emit_turn_stop_lifecycle(turn_context.extension_data.as_ref())
                .await;
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: turn_context.sub_id.clone(),
                last_agent_message,
                error,
                started_at,
                completed_at,
                duration_ms,
                time_to_first_token_ms,
            })
        };
        // Publish the terminal event only after the active turn is cleared, and keep the durable
        // context permit through the terminal-event flush. A client may submit rollback as soon as
        // it receives TurnComplete; that rollback must observe both settled live state and the
        // complete durable turn rather than racing this cleanup.
        let _durable_context_permit = match self.acquire_durable_context_permit().await {
            Ok(permit) => Some(permit),
            Err(err) => {
                warn!("failed to reserve durable context while completing turn: {err}");
                None
            }
        };
        let cleared_active_turn = {
            let mut active = self.active_turn.lock().await;
            if let Some(active_turn) = active.as_ref()
                && active_turn.task.is_none()
                && Arc::ptr_eq(&active_turn.turn_state, &turn_state)
            {
                *active = None;
                true
            } else {
                false
            }
        };
        if cleared_active_turn {
            self.active_turn_transition.notify_waiters();
        }
        self.send_event(turn_context.as_ref(), event).await;
        self.services
            .guardian_rejection_circuit_breaker
            .lock()
            .await
            .clear_turn(&turn_context.sub_id);
        // Regular items were flushed before this terminal event was appended; buffering
        // thread writers may not flush it without another explicit barrier.
        if let Err(err) = self.flush_rollout().await {
            warn!("failed to flush rollout after emitting terminal turn event: {err}");
        }
        drop(_durable_context_permit);
        if !cleared_active_turn {
            return TaskFinishAction::Finish;
        }
        if !queued_pending_input_after_mcp_use {
            self.emit_thread_idle_lifecycle_if_idle(idle_cause).await;
        }
        self.maybe_start_turn_for_pending_work().await;
        TaskFinishAction::Finish
    }

    pub(crate) async fn close_unified_exec_processes(&self) {
        self.services
            .unified_exec_manager
            .terminate_all_processes()
            .await;
    }

    pub(crate) async fn list_background_terminals(&self) -> Vec<BackgroundTerminalInfo> {
        self.services.unified_exec_manager.list_processes().await
    }

    pub(crate) async fn terminate_background_terminal(&self, process_id: i32) -> bool {
        self.services
            .unified_exec_manager
            .terminate_process(process_id)
            .await
    }

    async fn handle_task_abort(
        self: &Arc<Self>,
        active_turn: &ActiveTurn,
        mut task: RunningTask,
        reason: TurnAbortReason,
    ) -> Option<TurnAbortedEvent> {
        let sub_id = task.turn_context.sub_id.clone();
        if task.cancellation_token.is_cancelled() {
            return None;
        }

        if let Some(sender) = task.input_persisted.take() {
            let _ = sender.send(Err(
                TryStartTurnIfIdleRejectionReason::TaskEndedBeforePersistence,
            ));
        }
        trace!(task_kind = ?task.kind, sub_id, "aborting running task");
        task.cancellation_token.cancel();
        if reason == TurnAbortReason::Interrupted
            && task
                .turn_context
                .config
                .features
                .enabled(Feature::CodeModeInterrupt)
        {
            self.services
                .code_mode_service
                .interrupt_active_cells()
                .await;
        }
        task.turn_context
            .turn_metadata_state
            .cancel_git_enrichment_task();
        let session_task = Arc::clone(&task.task);

        let startup_was_complete = task.startup.is_complete();
        let mut completed = wait_for_task_completion_with_grace(&task).await;
        if !completed && !startup_was_complete {
            // Queue response handling commits on the source thread after exact target admission.
            // Let that tracked startup publish the admitted turn before forced abort can discard
            // its input or queue provenance, then give the cancelled task its ordinary grace
            // window to record input and stop.
            task.startup.wait().await;
            completed = wait_for_task_completion_with_grace(&task).await;
        }
        if !completed {
            warn!(
                "task {sub_id} didn't complete gracefully after \
                 {GRACEFULL_INTERRUPTION_TIMEOUT_MS}ms"
            );
        }

        task.handle.abort();

        session_task
            .abort(Arc::clone(self), Arc::clone(&task.turn_context))
            .await;

        let pending_prompt_input = {
            let mut turn_state = active_turn.turn_state.lock().await;
            let pending_input = turn_state.take_pending_input();
            let (pending_prompt_input, retained_input): (Vec<_>, Vec<_>) =
                pending_input.into_iter().partition(TurnInput::is_prompt);
            turn_state.prepend_pending_input(retained_input);
            pending_prompt_input
        };
        if reason == TurnAbortReason::Interrupted {
            // The user interrupted specifically to release already-accepted steer input from a
            // blocked tool call. Carry that input into one automatic continuation turn; recording
            // it in the aborted turn would leave no pending work to wake the model and would force
            // clients to submit the same user message a second time.
            self.input_queue
                .queue_turn_inputs_for_next_turn(pending_prompt_input)
                .await;
        } else {
            // Non-interrupt aborts do not automatically continue. Preserve their accepted prompt
            // prefix before clear_pending removes the task-local queue.
            run_hooks_and_record_inputs(
                self,
                &task.turn_context,
                &pending_prompt_input,
                PersistContext::Standard,
            )
            .await;
        }
        if reason == TurnAbortReason::Interrupted
            && let Some(marker) = interrupted_turn_history_marker(
                InterruptedTurnHistoryMarker::from_config_and_version(
                    task.turn_context.config.as_ref(),
                    task.turn_context.multi_agent_version,
                ),
            )
        {
            self.record_conversation_items(
                task.turn_context.as_ref(),
                std::slice::from_ref(&marker),
            )
            .await;
            // Ensure the marker is durably visible before emitting TurnAborted: some clients
            // synchronously re-read the rollout on receipt of the abort event.
            if let Err(err) = self.flush_rollout().await {
                warn!("failed to flush interrupted-turn marker before emitting TurnAborted: {err}");
            }
        }

        if reason == TurnAbortReason::Interrupted {
            run_turn_interrupt_hooks(self, &task.turn_context, active_turn.turn_state.as_ref())
                .await;
        }

        let started_at = task
            .turn_context
            .turn_timing_state
            .started_at_unix_secs()
            .await;
        let (completed_at, duration_ms, profile) = task
            .turn_context
            .turn_timing_state
            .complete_profile_and_duration_ms()
            .await;
        self.services
            .analytics_events_client
            .track_turn_profile(TurnProfileFact {
                turn_id: task.turn_context.sub_id.clone(),
                profile,
            });
        Some(TurnAbortedEvent {
            turn_id: Some(task.turn_context.sub_id.clone()),
            reason,
            started_at,
            completed_at,
            duration_ms,
        })
    }

    async fn publish_task_abort_event(
        &self,
        turn_context: &TurnContext,
        event: Option<TurnAbortedEvent>,
    ) {
        let Some(event) = event else {
            return;
        };
        // Clients may submit the interrupted steer as soon as they receive this event. Publish it
        // only after the caller has removed the taskless active-turn placeholder so the follow-up
        // is admitted as a new turn instead of racing stale turn state.
        self.send_event(turn_context, EventMsg::TurnAborted(event))
            .await;
        self.services
            .guardian_rejection_circuit_breaker
            .lock()
            .await
            .clear_turn(&turn_context.sub_id);
        // Regular items were flushed before this terminal event was appended; buffering
        // thread writers may not flush it without another explicit barrier.
        if let Err(err) = self.flush_rollout().await {
            warn!("failed to flush rollout after emitting terminal turn event: {err}");
        }
    }
}

fn is_mcp_server_use_context_input(item: &TurnInput) -> bool {
    let TurnInput::ResponseItem(item) = item else {
        return false;
    };
    crate::context::McpServerUseInstructions::matches_response_item(item)
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
