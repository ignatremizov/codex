//! Shell result admission, model delivery, and canonical history publication.

use super::TurnInput;
use super::session::Session;
use super::transcript_publication::ConversationBoundary;
use super::turn_context::TurnContext;
use codex_history::ResponseItemEnvelope;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::UserShellCommandFinalDelivery;
use codex_protocol::turn_input::TurnStartOptions;
use std::sync::atomic::Ordering;

impl Session {
    /// Accepts a result once, without retrying an ambiguous publication or starting work under locks.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "shell delivery must serialize admission and active-task finalization"
    )]
    pub(crate) async fn deliver_user_shell_result(
        &self,
        item: ResponseItem,
        turn_context: &TurnContext,
        final_delivery: UserShellCommandFinalDelivery,
        completion: &super::AcceptedCompletionDelivery,
    ) -> CodexResult<bool> {
        if final_delivery == UserShellCommandFinalDelivery::PresentationOnly {
            return Ok(false);
        }
        let _admission = self
            .submission_admission
            .admit_completion(completion)
            .await?;
        self.check_history_publication()?;
        let one_shot = self
            .app_server_client_metadata()
            .await
            .client_name
            .as_deref()
            == Some("codex_exec");
        // Publish before handing work to a cancellable model turn. A task-local or next-turn
        // queue is not durable ownership: interruption/shutdown can discard it without sampling.
        // The owned publisher appends, flushes, and installs the result exactly once. The queue
        // below contains only a continuation signal, never a second copy of the result payload.
        let (items, preparations) = self
            .prepare_conversation_items_for_history(
                turn_context,
                turn_context.model_info(),
                std::slice::from_ref(&item),
            )
            .await;
        self.record_prepared_conversation_items(
            turn_context,
            turn_context.model_info(),
            items
                .into_owned()
                .into_iter()
                .map(ResponseItemEnvelope::new)
                .collect(),
            preparations,
            /*acknowledgement*/ None,
            ConversationBoundary::Existing,
        )
        .await?;
        // Do not hold active-turn state across publication. Recheck its exact current owner only
        // after acknowledgement, so completion cannot enqueue into a turn that already ended.
        let active = self.active_turn.lock().await;
        // A previously accepted producer may finish after shutdown has closed fresh input.
        // Its result remains canonical, but cannot enter a closing task or start another turn.
        let closing = self.submission_admission.completion_is_closing();
        if !closing && let Some(active_turn) = active.as_ref()
            && active_turn.task.as_ref().is_some_and(|task| {
                !task.cancellation_token.is_cancelled()
                    && task.task.supports_pending_input_continuation()
                    && task.accepting_pending_input.load(Ordering::Acquire)
            })
        {
            self.input_queue
                .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                    active_turn.turn_state.as_ref(),
                    vec![TurnInput::UserShellContextReady],
                )
                .await;
            return Ok(false);
        }
        if final_delivery == UserShellCommandFinalDelivery::Wake && !one_shot && !closing {
            self.input_queue
                .queue_turn_inputs_for_next_turn(
                    vec![TurnInput::UserShellContextReady],
                    TurnStartOptions {
                        turn_trigger: Some("user_shell_wake".to_string()),
                        ..Default::default()
                    },
                )
                .await;
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
#[path = "user_shell_delivery_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "user_shell_scheduling_tests.rs"]
mod scheduling_tests;
