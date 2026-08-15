use super::input_queue::TurnInput;
use super::session::Session;
use super::turn_context::TurnContext;
use crate::codex_thread::TryStartTurnIfIdleError;
use crate::codex_thread::TryStartTurnIfIdleRejectionReason;
use crate::state::ActiveTurn;
use crate::state::TurnState;
use crate::tasks::MailboxParentProvenance;
use crate::tasks::RegularTask;
use codex_features::Feature;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::config_types::ModeKind;
use codex_protocol::models::ResponseItem;
use std::sync::Arc;

impl Session {
    /// Returns the input if there is no active turn to inject into.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn inject_if_running(
        &self,
        input: Vec<ResponseItem>,
    ) -> Result<(), Vec<ResponseItem>> {
        let mut active = self.active_turn.lock().await;
        match active.as_mut() {
            Some(active_turn) => {
                self.input_queue
                    .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                        active_turn.turn_state.as_ref(),
                        input
                            .into_iter()
                            .map(ResponseItemEnvelope::new)
                            .map(TurnInput::ResponseItem)
                            .collect(),
                    )
                    .await;
                Ok(())
            }
            None => Err(input),
        }
    }

    /// Injects hook context into the running turn atomically.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn provenance and turn state updates must remain atomic"
    )]
    pub(crate) async fn inject_hook_context_if_running(
        &self,
        input: Vec<ResponseItem>,
    ) -> Result<(), Vec<ResponseItem>> {
        let mut active = self.active_turn.lock().await;
        let Some(active_turn) = active.as_mut() else {
            return Err(input);
        };
        if active_turn.task.is_none() {
            return Err(input);
        }
        self.input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                active_turn.turn_state.as_ref(),
                input
                    .into_iter()
                    .map(ResponseItemEnvelope::new)
                    .map(TurnInput::ResponseItem)
                    .collect(),
            )
            .await;
        Ok(())
    }

    /// Starts a regular turn with the provided input only if automatic idle work
    /// is allowed for the current session state.
    ///
    /// This is the shared gate for extension-initiated idle work. It refuses to
    /// start a turn when user/client-triggered work or a subscribed agent wake
    /// is pending, any task is still active, or the session is currently in
    /// Plan mode. Active Review tasks are covered by the active-task check
    /// because Review turns are not steerable.
    pub(crate) async fn try_start_turn_if_idle(
        self: &Arc<Self>,
        input: Vec<TurnInput>,
    ) -> Result<(), TryStartTurnIfIdleError> {
        self.try_start_turn_if_idle_with_lease(input, ()).await
    }

    pub(crate) async fn try_start_turn_if_idle_with_lease(
        self: &Arc<Self>,
        input: Vec<TurnInput>,
        reservation_lease: impl Send,
    ) -> Result<(), TryStartTurnIfIdleError> {
        if input.is_empty() {
            return Ok(());
        }
        let has_prompt_input = input.iter().any(|item| {
            matches!(
                item,
                TurnInput::UserInput { content, .. } | TurnInput::AgentInput { content, .. }
                    if !content.is_empty()
            )
        });
        if self.input_queue.has_trigger_turn_mailbox_items().await {
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PendingTriggerTurn,
                input,
            ));
        }
        if !has_prompt_input && self.collaboration_mode().await.mode == ModeKind::Plan {
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PlanMode,
                input,
            ));
        }

        // Linearize automatic work against response delivery and late target-turn binding.
        // Lifecycle delivery takes the destination mailbox before the observer transaction, so
        // idle reservation follows the same order. Whichever side publishes first owns the next
        // turn: an already-bound `f` wake wins, while a policy bound after the placeholder is
        // installed may steer the automatic turn that was already reserved.
        let Ok(_mailbox_submission_permit) = self
            .services
            .agent_control
            .acquire_mailbox_submission_permit(self.thread_id)
            .await
        else {
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::Busy,
                input,
            ));
        };
        let _response_observation_transaction = self
            .services
            .agent_control
            .acquire_response_observation_transaction(self.presentation_id())
            .await;
        if self.input_queue.has_trigger_turn_mailbox_items().await
            || self
                .services
                .agent_control
                .has_bound_final_response_wake(self.presentation_id())
        {
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PendingTriggerTurn,
                input,
            ));
        }

        let turn_state = {
            let mut active_turn = self.active_turn.lock().await;
            if active_turn.is_some() {
                return Err(TryStartTurnIfIdleError::new(
                    TryStartTurnIfIdleRejectionReason::Busy,
                    input,
                ));
            }
            let active_turn = active_turn.get_or_insert_with(ActiveTurn::default);
            Arc::clone(&active_turn.turn_state)
        };
        drop(_response_observation_transaction);
        drop(_mailbox_submission_permit);
        // The active-turn placeholder now prevents another turn from starting. Release any
        // extension-owned state lease before turn-start lifecycle runs so contributors may
        // reacquire their own state locks without deadlocking.
        drop(reservation_lease);

        if self.input_queue.has_trigger_turn_mailbox_items().await {
            self.clear_reserved_idle_turn(&turn_state).await;
            self.maybe_start_turn_for_pending_work().await;
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PendingTriggerTurn,
                input,
            ));
        }

        let turn_context = self
            .new_turn_with_default_settings(uuid::Uuid::new_v4().to_string(), Default::default())
            .await;
        if !has_prompt_input && turn_context.mode() == ModeKind::Plan {
            self.clear_reserved_idle_turn(&turn_state).await;
            self.maybe_start_turn_for_pending_work().await;
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PlanMode,
                input,
            ));
        }
        self.maybe_emit_model_warnings_for_turn(turn_context.as_ref())
            .await;
        if self.input_queue.has_trigger_turn_mailbox_items().await {
            self.clear_reserved_idle_turn(&turn_state).await;
            self.maybe_start_turn_for_pending_work().await;
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PendingTriggerTurn,
                input,
            ));
        }
        let still_reserved = {
            let active_turn = self.active_turn.lock().await;
            active_turn.as_ref().is_some_and(|active_turn| {
                active_turn.task.is_none() && Arc::ptr_eq(&active_turn.turn_state, &turn_state)
            })
        };
        if !still_reserved {
            self.clear_reserved_idle_turn(&turn_state).await;
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::Busy,
                input,
            ));
        }

        let (input_persisted_sender, input_persisted_receiver) =
            has_prompt_input.then(tokio::sync::oneshot::channel).unzip();
        let original_input = input.clone();
        let task_input = if has_prompt_input {
            self.clear_connector_selection().await;
            for item in &input {
                if let TurnInput::UserInput { content, .. } = item {
                    turn_context.session_telemetry.user_prompt(content);
                }
            }
            input
        } else {
            self.input_queue
                .extend_pending_input_for_turn_state(turn_state.as_ref(), input)
                .await;
            Vec::new()
        };
        self.start_task(
            turn_context,
            task_input,
            RegularTask::new(),
            input_persisted_sender,
            MailboxParentProvenance::Ignore,
        )
        .await;
        if let Some(receiver) = input_persisted_receiver {
            return receiver
                .await
                .unwrap_or(Err(
                    TryStartTurnIfIdleRejectionReason::TaskEndedBeforePersistence,
                ))
                .map_err(|reason| TryStartTurnIfIdleError::new(reason, original_input));
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

    pub(crate) fn annotate_client_response_item(&self, item: ResponseItem) -> ResponseItemEnvelope {
        let metadata = (self.enabled(Feature::RetainClientDeveloperMessages)
            && matches!(&item, ResponseItem::Message { role, .. } if role == "developer"))
        .then_some(CodexHarnessMetadata {
            client_authored: true,
            ..Default::default()
        });

        ResponseItemEnvelope { item, metadata }
    }

    /// Preserves trusted client provenance while items wait for an active turn.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn inject_client_response_items(
        &self,
        items: Vec<ResponseItem>,
        turn_context: &TurnContext,
    ) {
        let items = items
            .into_iter()
            .map(|item| self.annotate_client_response_item(item))
            .collect::<Vec<_>>();
        let mut active = self.active_turn.lock().await;
        if let Some(active_turn) = active.as_mut() {
            self.input_queue
                .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                    active_turn.turn_state.as_ref(),
                    items.into_iter().map(TurnInput::ResponseItem).collect(),
                )
                .await;
            return;
        }
        drop(active);
        self.record_annotated_conversation_items(turn_context, items)
            .await;
    }

    pub(crate) async fn record_annotated_conversation_items(
        &self,
        turn_context: &TurnContext,
        items: Vec<ResponseItemEnvelope>,
    ) {
        if items.iter().all(|item| item.metadata.is_none()) {
            let items = items
                .into_iter()
                .map(ResponseItemEnvelope::into_item)
                .collect::<Vec<_>>();
            self.record_conversation_items(turn_context, &items).await;
            return;
        }

        let mut annotated_items = Vec::with_capacity(items.len());
        let mut image_preparations = Vec::new();
        for envelope in items {
            let (prepared_items, prepared_images) = self.prepare_conversation_items_for_history(
                turn_context,
                std::slice::from_ref(&envelope.item),
            );
            image_preparations.extend(prepared_images);

            let mut metadata = envelope.metadata;
            annotated_items.extend(prepared_items.into_owned().into_iter().map(|item| {
                ResponseItemEnvelope {
                    item,
                    metadata: metadata.take(),
                }
            }));
        }
        self.record_prepared_conversation_items(turn_context, annotated_items, image_preparations)
            .await;
    }

    /// Returns the input if there is no active turn to inject into.
    pub async fn inject_response_items(
        &self,
        input: Vec<ResponseItem>,
    ) -> Result<(), Vec<ResponseItem>> {
        self.inject_if_running(input).await
    }

    /// Injects items into active work, or records them without starting a turn.
    pub(crate) async fn inject_no_new_turn(
        &self,
        items: Vec<ResponseItem>,
        current_turn_context: Option<&TurnContext>,
    ) {
        let Err(items) = self.inject_if_running(items).await else {
            return;
        };
        let default_turn_context;
        let turn_context = match current_turn_context {
            Some(turn_context) => turn_context,
            None => {
                default_turn_context = self.new_default_turn().await;
                default_turn_context.as_ref()
            }
        };
        if items
            .iter()
            .any(|item| matches!(item, ResponseItem::AgentMessage { .. }))
        {
            if let Err(err) = self
                .record_history_only_conversation_items(turn_context, &items)
                .await
            {
                tracing::error!("failed to record history-only agent message: {err}");
            }
        } else {
            self.record_conversation_items(turn_context, &items).await;
        }
    }
}
