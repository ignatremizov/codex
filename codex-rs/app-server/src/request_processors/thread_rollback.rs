//! Correlated Legacy rollback responses and exact-runtime quarantine.

use super::*;
use codex_app_server_protocol::THREAD_ROLLBACK_COMMITTED_ERROR_DATA_FIELD;
use codex_app_server_protocol::THREAD_ROLLBACK_REFRESH_REQUIRED_ERROR_DATA_FIELD;
use codex_app_server_protocol::ThreadRollbackParams;
use codex_app_server_protocol::ThreadRollbackResponse;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::ThreadHistoryMode;

pub(crate) struct PendingRollback {
    pub(crate) request_id: ConnectionRequestId,
    pub(crate) submission_id: String,
    pub(crate) listener_generation: u64,
}

impl thread_processor::ThreadRequestProcessor {
    pub(crate) async fn thread_rollback(
        &self,
        request_id: ConnectionRequestId,
        params: ThreadRollbackParams,
        app_server_client_name: Option<&str>,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        if app_server_client_name != Some("codex-tui") {
            self.send_deprecation_notice(
                request_id.connection_id,
                "thread/rollback is deprecated and will be removed soon",
            )
            .await;
        }
        if params.num_turns == 0 {
            return Err(invalid_request("num_turns must be >= 1"));
        }
        let (thread_id, thread) = self.load_thread(&params.thread_id).await?;
        if thread.config_snapshot().await.history_mode != ThreadHistoryMode::Legacy {
            return Err(invalid_request(
                "paginated threads do not support thread/rollback",
            ));
        }
        if matches!(
            self.ensure_conversation_listener(
                thread_id,
                request_id.connection_id,
                /*raw_events_enabled*/ false
            )
            .await?,
            EnsureConversationListenerResult::ConnectionClosed
        ) {
            return Err(internal_error(
                "connection closed before rollback submission",
            ));
        }
        let state = self.thread_state_manager.thread_state(thread_id).await;
        // The listener cannot consume the acknowledgement before its ID is registered.
        // Admission must not wait here: resume can hold the publication permit while
        // waiting for this listener, and another sender may be waiting on that permit.
        let mut state = state.lock().await;
        if state.pending_rollback.is_some() {
            return Err(invalid_request("thread rollback is already in progress"));
        }
        if !state.listener_matches(&thread) {
            return Err(internal_error(
                "thread listener changed before rollback submission",
            ));
        }
        let op = if params.expected_start_turn_id.is_some() || params.expected_turn_count.is_some()
        {
            Op::ThreadRollbackMaterialized {
                num_turns: params.num_turns,
                expected_start_turn_id: params.expected_start_turn_id,
                expected_turn_count: params.expected_turn_count,
            }
        } else {
            Op::ThreadRollback {
                num_turns: params.num_turns,
            }
        };
        let submission_id = thread
            .try_submit(op)
            .map_err(|error| invalid_request(error.to_string()))?;
        state.pending_rollback = Some(PendingRollback {
            request_id,
            submission_id,
            listener_generation: state.listener_generation,
        });
        Ok(None)
    }
}

fn outcome_error(message: impl Into<String>, field: &str) -> JSONRPCErrorError {
    let mut error = internal_error(message);
    error.data = Some(serde_json::json!({(field): true}));
    error
}

/// Runs on the same listener as the preceding transcript events.
#[expect(
    clippy::await_holding_invalid_type,
    reason = "generation validation must cover exact-runtime removal and response publication"
)]
pub(super) async fn handle_event(
    event: &Event,
    thread_id: ThreadId,
    conversation: &Arc<CodexThread>,
    manager: &Arc<ThreadManager>,
    state: &Arc<Mutex<ThreadState>>,
    outgoing: &Arc<OutgoingMessageSender>,
    config: &Config,
) -> bool {
    let failure = match &event.msg {
        EventMsg::ThreadRolledBack(_) => None,
        EventMsg::Error(error)
            if matches!(
                error.codex_error_info,
                Some(
                    CodexErrorInfo::ThreadRollbackFailed
                        | CodexErrorInfo::ThreadRollbackCommitUnknown
                )
            ) =>
        {
            Some(error)
        }
        _ => return false,
    };
    let pending = {
        let mut guard = state.lock().await;
        if !guard.listener_matches(conversation)
            || !guard.pending_rollback.as_ref().is_some_and(|pending| {
                pending.submission_id == event.id
                    && pending.listener_generation == guard.listener_generation
            })
        {
            return true;
        }
        if matches!(&event.msg, EventMsg::ThreadRolledBack(_)) {
            guard.reset_after_rollback();
        }
        guard.pending_rollback.take()
    };
    let Some(pending) = pending else { return true };
    if let Some(error) = failure {
        let response =
            if error.codex_error_info == Some(CodexErrorInfo::ThreadRollbackCommitUnknown) {
                // A timeout does not release a writer. Leave the poisoned runtime registered.
                let terminated = tokio::time::timeout(
                    Duration::from_secs(10),
                    conversation.wait_until_terminated(),
                )
                .await
                .is_ok();
                if terminated && conversation.rollback_reload_ready() {
                    let mut guard = state.lock().await;
                    if guard.listener_generation == pending.listener_generation
                        && guard.listener_matches(conversation)
                    {
                        guard.reset_after_rollback();
                        // Retain state, watch registration and subscriptions: a subsequent
                        // resume replaces this stopped listener. Map-wide cleanup could
                        // delete a replacement registered during an awaited teardown.
                        let _ = manager
                            .remove_thread_if_matches(&thread_id, conversation)
                            .await;
                    }
                }
                outcome_error(
                    error.message.clone(),
                    THREAD_ROLLBACK_REFRESH_REQUIRED_ERROR_DATA_FIELD,
                )
            } else {
                invalid_request(error.message.clone())
            };
        outgoing.send_error(pending.request_id, response).await;
        return true;
    }

    let response = async {
        // Core's acknowledgement follows canonical marker/repair publication.
        // Do not reacquire its permit here: running resume may own that permit
        // while asking this same listener to drain the acknowledgement.
        let stored = conversation
            .read_thread(
                /*include_archived*/ false, /*include_history*/ true,
            )
            .await
            .map_err(|error| error.to_string())?;
        let (mut thread, history) =
            thread_from_stored_thread(stored, &config.model_provider_id, &config.cwd);
        let history =
            history.ok_or_else(|| "canonical rollback history is unavailable".to_string())?;
        thread.turns = build_legacy_api_turns_from_rollout_items(&history.items);
        thread.session_id = conversation.startup_metadata().session_id.to_string();
        apply_live_thread_settings(&mut thread, &conversation.config_snapshot().await);
        let guard = state.lock().await;
        if guard.listener_generation != pending.listener_generation
            || !guard.listener_matches(conversation)
        {
            return Err("thread listener changed after rollback committed".to_string());
        }
        outgoing
            .send_response(
                pending.request_id.clone(),
                ThreadRollbackResponse { thread },
            )
            .await;
        Ok(())
    }
    .await;
    if let Err(error) = response {
        outgoing
            .send_error(
                pending.request_id,
                outcome_error(
                    format!("rollback committed but canonical response hydration failed: {error}"),
                    THREAD_ROLLBACK_COMMITTED_ERROR_DATA_FIELD,
                ),
            )
            .await;
    }
    true
}
