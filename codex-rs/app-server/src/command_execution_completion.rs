//! Approval-owned command lifecycles are revalidated at the output queue boundary.

use super::CommandExecutionCompletionItem;
use super::now_unix_timestamp_ms;
use crate::outgoing_message::ThreadScopedOutgoingMessageSender;
use crate::thread_state::CommandExecutionCompletionState;
use crate::thread_state::CommandExecutionStartReceipt;
use crate::thread_state::ThreadState;
use codex_app_server_protocol::CommandExecutionSource;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_core::CodexThread;
use codex_protocol::ThreadId;
use std::sync::Arc;
use tokio::sync::Mutex;

/// The event listener's identity, captured before awaiting root-approval eligibility.
pub(super) struct CommandApprovalOrigin {
    conversation: Arc<CodexThread>,
    listener_generation: u64,
    turn_id: String,
}

impl CommandApprovalOrigin {
    pub(super) fn capture(
        state: &ThreadState,
        conversation: &Arc<CodexThread>,
        turn_id: &str,
    ) -> Option<Self> {
        (state.listener_matches(conversation) && state.active_turn_id() == Some(turn_id)).then(
            || Self {
                conversation: Arc::clone(conversation),
                listener_generation: state.listener_generation,
                turn_id: turn_id.to_owned(),
            },
        )
    }
}

pub(super) async fn start_root_command_execution_item(
    origin: CommandApprovalOrigin,
    conversation_id: &ThreadId,
    item_id: &str,
    item: &CommandExecutionCompletionItem,
    outgoing: &ThreadScopedOutgoingMessageSender,
    thread_state: &Arc<Mutex<ThreadState>>,
) -> Option<CommandExecutionStartReceipt> {
    let receipt = {
        let mut state = thread_state.lock().await;
        if state.listener_generation != origin.listener_generation
            || !state.listener_matches(&origin.conversation)
            || state.active_turn_id() != Some(origin.turn_id.as_str())
            || !state
                .turn_summary
                .command_execution_started
                .insert(item_id.to_owned())
        {
            return None;
        }
        let receipt = CommandExecutionStartReceipt {
            conversation: Arc::downgrade(&origin.conversation),
            listener_generation: origin.listener_generation,
            turn_id: origin.turn_id.clone(),
            token: Arc::new(()),
            completion: CommandExecutionCompletionState::Pending,
        };
        state
            .turn_summary
            .command_execution_receipts
            .insert(item_id.to_owned(), receipt.clone());
        receipt
    };
    let notification = ServerNotification::ItemStarted(ItemStartedNotification {
        thread_id: conversation_id.to_string(),
        turn_id: origin.turn_id.clone(),
        started_at_ms: now_unix_timestamp_ms(),
        deadline_at_ms: None,
        item: ThreadItem::CommandExecution {
            id: item_id.to_owned(),
            model_context: item.model_context.clone(),
            plugin_id: item.plugin_id.clone(),
            script_path: item.script_path.clone(),
            command: item.command.clone(),
            cwd: item.cwd.clone(),
            process_id: None,
            source: CommandExecutionSource::Agent,
            user_shell_response_handling: None,
            status: CommandExecutionStatus::InProgress,
            command_actions: item.command_actions.clone(),
            aggregated_output: None,
            exit_code: None,
            duration_ms: None,
        },
    });
    outgoing
        .send_server_notification_guarded(notification, thread_state, |state| {
            receipt_matches(
                state,
                &origin.conversation,
                item_id,
                &origin.turn_id,
                &receipt,
                CommandExecutionCompletionState::Pending,
            )
        })
        .await;
    let mut state = thread_state.lock().await;
    if receipt_matches(
        &state,
        &origin.conversation,
        item_id,
        &origin.turn_id,
        &receipt,
        CommandExecutionCompletionState::Pending,
    ) {
        return Some(receipt);
    }
    if state
        .turn_summary
        .command_execution_receipts
        .get(item_id)
        .is_some_and(|stored| Arc::ptr_eq(&stored.token, &receipt.token))
    {
        state
            .turn_summary
            .command_execution_receipts
            .remove(item_id);
        state.turn_summary.command_execution_started.remove(item_id);
    }
    None
}

fn receipt_matches(
    state: &ThreadState,
    conversation: &Arc<CodexThread>,
    item_id: &str,
    turn_id: &str,
    receipt: &CommandExecutionStartReceipt,
    completion: CommandExecutionCompletionState,
) -> bool {
    state
        .turn_summary
        .command_execution_receipts
        .get(item_id)
        .is_some_and(|stored| {
            Arc::ptr_eq(&stored.token, &receipt.token)
                && stored.completion == completion
                && stored.listener_generation == receipt.listener_generation
                && stored.listener_generation == state.listener_generation
                && stored.turn_id == turn_id
                && receipt.turn_id == turn_id
                && receipt
                    .conversation
                    .upgrade()
                    .is_some_and(|origin| Arc::ptr_eq(&origin, conversation))
                && state.listener_matches(conversation)
                && state.active_turn_id() == Some(turn_id)
                && state
                    .turn_summary
                    .command_execution_started
                    .contains(item_id)
        })
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn complete_command_execution_item(
    conversation_id: &ThreadId,
    turn_id: String,
    item_id: String,
    completion_item: CommandExecutionCompletionItem,
    process_id: Option<String>,
    source: CommandExecutionSource,
    status: CommandExecutionStatus,
    conversation: Option<&Arc<CodexThread>>,
    expected_receipt: Option<CommandExecutionStartReceipt>,
    outgoing: &ThreadScopedOutgoingMessageSender,
    thread_state: &Arc<Mutex<ThreadState>>,
) {
    {
        let mut state = thread_state.lock().await;
        if let Some(receipt) = &expected_receipt {
            let Some(conversation) = conversation else {
                return;
            };
            if !receipt_matches(
                &state,
                conversation,
                &item_id,
                &turn_id,
                receipt,
                CommandExecutionCompletionState::Pending,
            ) {
                return;
            }
            // Keep the exact token in the existing lifecycle collection while output waits.
            // Canonical completion, turn reset, or replacement invalidates that authority.
            if let Some(stored) = state
                .turn_summary
                .command_execution_receipts
                .get_mut(&item_id)
            {
                stored.completion = CommandExecutionCompletionState::Publishing;
            }
        } else if !state
            .turn_summary
            .command_execution_started
            .remove(&item_id)
        {
            return;
        } else {
            state
                .turn_summary
                .command_execution_receipts
                .remove(&item_id);
        }
    }

    let notification = ServerNotification::ItemCompleted(ItemCompletedNotification {
        thread_id: conversation_id.to_string(),
        turn_id: turn_id.clone(),
        completed_at_ms: now_unix_timestamp_ms(),
        item: ThreadItem::CommandExecution {
            id: item_id.clone(),
            model_context: completion_item.model_context,
            plugin_id: completion_item.plugin_id,
            script_path: completion_item.script_path,
            command: completion_item.command,
            cwd: completion_item.cwd,
            process_id,
            source,
            user_shell_response_handling: None,
            status,
            command_actions: completion_item.command_actions,
            aggregated_output: None,
            exit_code: None,
            duration_ms: None,
        },
    });
    if let Some(receipt) = expected_receipt {
        outgoing
            .send_server_notification_guarded(notification, thread_state, |state| {
                conversation.is_some_and(|conversation| {
                    receipt_matches(
                        state,
                        conversation,
                        &item_id,
                        &turn_id,
                        &receipt,
                        CommandExecutionCompletionState::Publishing,
                    )
                })
            })
            .await;
        // Also retire our claim when there are no subscribers or the output channel closes.
        // Never remove a new same-ID lifecycle, even if the old publication was rejected.
        let mut state = thread_state.lock().await;
        if state
            .turn_summary
            .command_execution_receipts
            .get(&item_id)
            .is_some_and(|stored| Arc::ptr_eq(&stored.token, &receipt.token))
        {
            state
                .turn_summary
                .command_execution_receipts
                .remove(&item_id);
            state
                .turn_summary
                .command_execution_started
                .remove(&item_id);
        }
    } else {
        outgoing.send_server_notification(notification).await;
    }
}

#[cfg(test)]
#[path = "command_execution_completion_tests.rs"]
mod tests;
