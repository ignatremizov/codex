//! Legacy append-only rollback at the shared history-publication boundary.

use std::sync::Arc;

use codex_app_server_protocol::materialized_rollback_start;
use codex_history::RolloutItem;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadRolledBackEvent;

use super::Session;
use crate::thread_rollout_truncation::instruction_positions_in_rollout;

enum Target {
    Instructions(u32),
    Materialized {
        count: u32,
        expected_start_turn_id: Option<String>,
        expected_turn_count: Option<u32>,
    },
}

/// Abandoning a submitted marker cannot reopen live mutation admission.
struct MutationReceipt<'a> {
    session: &'a Session,
    completed: bool,
}

impl Drop for MutationReceipt<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.session
                .quarantine_history("rollback transaction lost its receipt".to_string());
        }
    }
}

pub(crate) async fn thread_rollback(sess: &Arc<Session>, sub_id: String, num_turns: u32) -> bool {
    rollback(sess, sub_id, Target::Instructions(num_turns)).await
}

pub(crate) async fn thread_rollback_materialized(
    sess: &Arc<Session>,
    sub_id: String,
    num_turns: u32,
    expected_start_turn_id: Option<String>,
    expected_turn_count: Option<u32>,
) -> bool {
    rollback(
        sess,
        sub_id,
        Target::Materialized {
            count: num_turns,
            expected_start_turn_id,
            expected_turn_count,
        },
    )
    .await
}

async fn rollback(sess: &Arc<Session>, sub_id: String, target: Target) -> bool {
    let result = apply(sess, target).await;
    let reload = sess.submission_admission.requires_reload();
    if !reload {
        sess.submission_admission.rollback_completed(&sub_id);
    }
    let msg = match result {
        Ok(marker) => EventMsg::ThreadRolledBack(marker),
        Err(error) => EventMsg::Error(ErrorEvent {
            misalignment: None,
            message: error.to_string(),
            codex_error_info: Some(if reload {
                CodexErrorInfo::ThreadRollbackCommitUnknown
            } else {
                CodexErrorInfo::ThreadRollbackFailed
            }),
        }),
    };
    // Neither rejection nor the acknowledgement is another canonical append.
    sess.deliver_event_raw(Event { id: sub_id, msg }).await;
    reload
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "the idle-turn reservation covers the complete rollback transaction"
)]
async fn apply(sess: &Arc<Session>, target: Target) -> CodexResult<ThreadRolledBackEvent> {
    let count = match &target {
        Target::Instructions(count) | Target::Materialized { count, .. } => *count,
    };
    if count == 0 {
        return Err(CodexErr::InvalidRequest(
            "num_turns must be >= 1".to_string(),
        ));
    }
    let permit = sess
        .acquire_history_publication_barrier()
        .await
        .inspect_err(|error| {
            sess.quarantine_history(format!(
                "rollback could not reserve healthy canonical history: {error}"
            ));
        })?;
    let active = sess.active_turn.lock().await;
    if active.is_some() {
        return Err(CodexErr::InvalidRequest(
            "Cannot rollback while a turn is in progress.".to_string(),
        ));
    }
    let live = sess
        .live_thread_for_persistence("rollback thread")
        .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
    if sess.state.lock().await.session_configuration.history_mode != ThreadHistoryMode::Legacy {
        return Err(CodexErr::InvalidRequest(
            "thread/rollback only supports Legacy history".to_string(),
        ));
    }
    live.flush_canonical().await.map_err(|error| {
        sess.quarantine_history(format!(
            "rollback preflight history barrier failed: {error}"
        ));
        CodexErr::Fatal(error.to_string())
    })?;
    let stored = live
        .load_history(/*include_archived*/ false)
        .await
        .map_err(|error| CodexErr::Fatal(error.to_string()))?;
    if stored.thread_id != sess.thread_id()
        || stored
            .items
            .iter()
            .find_map(|item| match item {
                RolloutItem::SessionMeta(meta) => Some(&meta.meta),
                _ => None,
            })
            .is_some_and(|meta| {
                meta.id != sess.thread_id() || meta.history_mode != ThreadHistoryMode::Legacy
            })
    {
        return Err(CodexErr::InvalidRequest(
            "canonical history does not belong to this Legacy thread".to_string(),
        ));
    }
    let instructions = instruction_positions_in_rollout(&stored.items);
    let (num_turns, materialized_turns, start) = match target {
        Target::Instructions(count) => {
            let boundary = instructions
                .len()
                .saturating_sub(usize::try_from(count).unwrap_or(usize::MAX));
            let start = instructions.get(boundary).copied().map(|instruction| {
                stored.items[..=instruction]
                    .iter()
                    .rposition(|item| {
                        matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(_)))
                    })
                    .filter(|start| {
                        !stored.items[*start..instruction].iter().any(|item| {
                            matches!(
                                item,
                                RolloutItem::EventMsg(
                                    EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
                                )
                            )
                        }) && !instructions[..boundary]
                            .iter()
                            .any(|position| position >= start)
                    })
                    .unwrap_or(instruction)
            });
            (count, None, start)
        }
        Target::Materialized {
            count,
            expected_start_turn_id,
            expected_turn_count,
        } => {
            let boundary = materialized_rollback_start(&stored.items, count).ok_or_else(|| {
                CodexErr::InvalidRequest("selected rollback suffix was not found".to_string())
            })?;
            if expected_start_turn_id
                .as_ref()
                .is_some_and(|id| *id != boundary.turn_id)
                || expected_turn_count
                    .is_some_and(|count| usize::try_from(count).ok() != Some(boundary.turn_count))
            {
                return Err(CodexErr::InvalidRequest(
                    "thread history changed after selecting the prompt; rollback was not applied"
                        .to_string(),
                ));
            }
            let count_instructions = instructions
                .iter()
                .filter(|index| **index >= boundary.rollout_index)
                .count();
            (
                u32::try_from(count_instructions).unwrap_or(u32::MAX),
                Some(count),
                Some(boundary.rollout_index),
            )
        }
    };
    let marker = ThreadRolledBackEvent {
        num_turns,
        materialized_turns,
        rollback_start_index: start.and_then(|index| u64::try_from(index).ok()),
    };
    // A history-only context preserves the selected environments and does not
    // reacquire the active-turn reservation held by this transaction.
    let turn_context = sess.new_history_only_turn().await;
    let mut replay = stored.items;
    let marker_item = RolloutItem::EventMsg(EventMsg::ThreadRolledBack(marker.clone()));
    replay.push(marker_item.clone());
    let prepared = sess
        .prepare_rollout_reconstruction(&turn_context, &replay)
        .await;
    let mut receipt = MutationReceipt {
        session: sess,
        completed: false,
    };
    // A singleton command is deliberately outside the recorder's normal retry buffer.
    if let Err(error) = live.append_items_and_flush_canonical(&[marker_item]).await {
        sess.quarantine_history(format!("rollback marker durability is unknown: {error}"));
        return Err(CodexErr::Fatal(format!(
            "rollback requires canonical reload: {error}"
        )));
    }
    let applied = match sess
        .install_rollout_reconstruction_with_permit(&turn_context, prepared, Vec::new(), permit)
        .await
    {
        Ok(applied) => applied,
        Err(error) => {
            sess.quarantine_history(format!(
                "rollback committed but reconstruction failed: {error}"
            ));
            return Err(error);
        }
    };
    let _ = sess
        .services
        .thread_extension_data
        .remove::<crate::context::NodeReplReviewEvidence>();
    sess.recompute_token_usage(&turn_context).await;
    if let Some(repair) = applied.repair.as_ref()
        && let Err(error) = sess.persist_reconstruction_repair_with_policy(repair).await
    {
        tracing::warn!(%error, "failed to persist optional rollback representation repair");
    }
    drop(active);
    receipt.completed = true;
    Ok(marker)
}
