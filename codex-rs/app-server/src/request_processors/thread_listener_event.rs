//! Shared listener event handling for normal delivery and bounded resume drains.

use super::response_item_transcript::emit_response_item_transcript_changes;
use super::*;
use codex_protocol::protocol::Event;

#[allow(clippy::too_many_arguments)]
pub(super) async fn process_thread_listener_event(
    event: Event,
    conversation_id: ThreadId,
    conversation: &Arc<CodexThread>,
    turn_cost_worker: Option<&crate::turn_cost_worker::TurnCostWorkerHandle>,
    thread_manager: &Arc<ThreadManager>,
    thread_state_manager: &ThreadStateManager,
    thread_state: &Arc<Mutex<ThreadState>>,
    thread_watch_manager: &ThreadWatchManager,
    outgoing: &Arc<OutgoingMessageSender>,
    config: &codex_core::config::Config,
) {
    if super::thread_rollback::handle_event(
        &event,
        conversation_id,
        conversation,
        thread_manager,
        thread_state,
        outgoing,
        config,
    )
    .await
    {
        return;
    }
    let shutdown_complete = matches!(&event.msg, EventMsg::ShutdownComplete);
    if let Some(worker) = turn_cost_worker {
        worker.observe_event(conversation_id, config, &event, || {
            conversation.session_telemetry()
        });
    }

    // Track the event before emitting any typed translations so thread-local state such as raw
    // event opt-in stays synchronized with the conversation.
    let (raw_events_enabled, transcript_item_changes) = {
        let mut thread_state = thread_state.lock().await;
        let changes = thread_state.track_current_turn_event(&event.id, &event.msg);
        let transcript_item_changes = if matches!(&event.msg, EventMsg::RawResponseItem(_)) {
            changes.changed_items
        } else {
            Vec::new()
        };
        (
            thread_state.experimental_raw_events,
            transcript_item_changes,
        )
    };
    let suppress_raw_event = matches!(
        &event.msg,
        EventMsg::RawResponseItem(_) | EventMsg::RawResponseCompleted(_)
    ) && !raw_events_enabled;
    if suppress_raw_event && transcript_item_changes.is_empty() {
        return;
    }

    let subscribed_connection_ids = thread_state_manager
        .subscribed_connection_ids(conversation_id)
        .await;
    let thread_outgoing = ThreadScopedOutgoingMessageSender::new(
        Arc::clone(outgoing),
        subscribed_connection_ids,
        conversation_id,
    );

    if !suppress_raw_event {
        apply_bespoke_event_handling(
            event,
            conversation_id,
            Arc::clone(conversation),
            Arc::clone(thread_manager),
            thread_outgoing,
            Arc::clone(thread_state),
            thread_watch_manager.clone(),
        )
        .await;
    }
    if shutdown_complete
        && let Some(completion_tx) = thread_state.lock().await.take_shutdown_drain_waiter()
    {
        let _ = completion_tx.send(());
    }
    if !transcript_item_changes.is_empty() {
        let Some(_delivery_permit) = thread_state_manager
            .acquire_typed_transcript_delivery_permit(conversation_id)
            .await
        else {
            return;
        };
        let transcript_connection_ids = thread_state_manager
            .typed_transcript_connection_ids(conversation_id)
            .await;
        let transcript_outgoing = ThreadScopedOutgoingMessageSender::new(
            Arc::clone(outgoing),
            transcript_connection_ids,
            conversation_id,
        );
        emit_response_item_transcript_changes(
            conversation_id,
            transcript_item_changes,
            &transcript_outgoing,
        )
        .await;
    }
}
