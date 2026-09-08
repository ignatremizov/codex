//! Direct waits observe inventory hints; only the ordered result recorder claims mail.

use super::WaitAgentResult;
use super::WaitReturnReason;
use crate::agent::control::V1WaitStatusAuthority;
use crate::agent::status::is_final;
use crate::function_tool::FunctionCallError;
use crate::session::TerminalStatusEvent;
use crate::session::TerminalStatusSubscription;
use crate::session::mailbox::MailboxConsumption;
use crate::session::session::Session;
use codex_protocol::ThreadId;
use codex_thread_store::MailboxInvocation;
use codex_thread_store::MailboxSelection;
use futures::FutureExt;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::Instant;

/// Existing claims are ready for repair, including empty batches. Reading one
/// never establishes a new claim or permits subsequently accepted mail to join.
pub(super) async fn recover_wait_result(
    session: &Session,
    turn_id: &str,
    operation: &MailboxConsumption,
    mail_only_targets: &[ThreadId],
) -> Result<Option<WaitAgentResult>, FunctionCallError> {
    let Some(claim) = session
        .services
        .thread_store
        .lookup_mailbox_claim(MailboxInvocation {
            receiver_thread_id: session.thread_id,
            turn_id: turn_id.to_string(),
            tool_call_id: operation.tool_call_id.clone(),
        })
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to look up wait_agent mailbox claim: {err}"
            ))
        })?
    else {
        return Ok(None);
    };
    let same_selection = match (&claim.selection, &operation.selection) {
        (MailboxSelection::All, MailboxSelection::All) => true,
        (MailboxSelection::Senders(original), MailboxSelection::Senders(requested)) => {
            original.iter().all(|sender| requested.contains(sender))
                && requested.iter().all(|sender| original.contains(sender))
        }
        (MailboxSelection::All, MailboxSelection::Senders(_))
        | (MailboxSelection::Senders(_), MailboxSelection::All) => false,
    };
    if !same_selection {
        return Err(FunctionCallError::RespondToModel(
            "wait_agent invocation already has a different fixed mailbox selection".to_string(),
        ));
    }
    Ok(Some(WaitAgentResult {
        status: HashMap::new(),
        timed_out: false,
        return_reason: WaitReturnReason::Recovery,
        mail_only_targets: mail_only_targets.to_vec(),
    }))
}

/// UUID mail selection grants no lifecycle or status authority. Other selectors
/// retain the caller-root resolver, including its errors and closed aliases.
pub(super) async fn mail_only_target(
    session: &Arc<Session>,
    target: &str,
) -> Result<Option<ThreadId>, FunctionCallError> {
    if target == "user" {
        return Err(FunctionCallError::RespondToModel(
            "wait_agent targets must be agents; use check_mail to consume user mail".to_string(),
        ));
    }
    let Ok(id) = ThreadId::from_string(target.strip_prefix("id:").unwrap_or(target)) else {
        return Ok(None);
    };
    if id == session.thread_id {
        return Err(FunctionCallError::RespondToModel(
            "an agent cannot wait on itself; continue the current turn directly".to_string(),
        ));
    }
    let authority = session
        .services
        .agent_control
        .v1_wait_status_authority(id)
        .await
        .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
    Ok(match authority {
        V1WaitStatusAuthority::Controlled => None,
        V1WaitStatusAuthority::MailOnly => Some(id),
    })
}

async fn wait_for_final_status(
    session: Arc<Session>,
    thread_id: ThreadId,
    mut status_rx: TerminalStatusSubscription,
) -> Option<(ThreadId, TerminalStatusEvent)> {
    match status_rx.recv().await {
        Some(status) => Some((thread_id, status)),
        None => {
            let latest = session.services.agent_control.get_status(thread_id).await;
            is_final(&latest).then_some((
                thread_id,
                TerminalStatusEvent {
                    turn_id: None,
                    status: latest,
                },
            ))
        }
    }
}

pub(super) async fn wait_for_outcome(
    session: &Arc<Session>,
    status_rxs: Vec<(ThreadId, TerminalStatusSubscription)>,
    initial_statuses: Vec<(ThreadId, TerminalStatusEvent)>,
    deadline: Instant,
    selection: Option<&MailboxSelection>,
) -> Result<(Vec<(ThreadId, TerminalStatusEvent)>, WaitReturnReason), FunctionCallError> {
    // Subscribe before any inventory read. The watch is a versioned hint, not
    // payload storage; acceptance during a read remains visible to changed().
    let mut activity = session.subscribe_mailbox_activity();
    if !initial_statuses.is_empty() {
        return Ok((initial_statuses, WaitReturnReason::Completion));
    }
    let mut futures = FuturesUnordered::new();
    for (id, rx) in status_rxs {
        futures.push(wait_for_final_status(Arc::clone(session), id, rx));
    }
    let mut results = Vec::new();
    let mut refresh_inventory = selection.is_some();
    let mut mail_ready = false;
    loop {
        if refresh_inventory && let Some(selection) = selection {
            let inventory = tokio::select! {
                biased;
                result = futures.next(), if !futures.is_empty() => {
                    if let Some(Some(result)) = result {
                        results.push(result);
                        refresh_inventory = false;
                    }
                    continue;
                }
                inventory = session.services.thread_store.read_mailbox_inventory(session.thread_id) => {
                    inventory.map_err(|err| FunctionCallError::RespondToModel(format!(
                        "failed to read wait_agent mailbox inventory: {err}"
                    )))?
                }
                _ = tokio::time::sleep_until(deadline) => {
                    return Ok((results, WaitReturnReason::Timeout));
                }
            };
            mail_ready = inventory.pending_senders.iter().any(|entry| {
                entry.count > 0
                    && match selection {
                        MailboxSelection::All => true,
                        MailboxSelection::Senders(senders) => senders.contains(&entry.sender),
                    }
            });
            refresh_inventory = false;
        }
        // Poll terminal futures after the inventory await as well: a completion
        // that became ready during that read wins over mail and timeout.
        loop {
            match futures.next().now_or_never() {
                Some(Some(Some(result))) => results.push(result),
                Some(Some(None)) => continue,
                Some(None) | None => break,
            }
        }
        if !results.is_empty() {
            return Ok((results, WaitReturnReason::Completion));
        }
        if mail_ready {
            return Ok((results, WaitReturnReason::Mail));
        }
        // Preserve nested completion-only exhaustion behavior. Direct mail-only
        // waits must remain subscribed even without any terminal-status futures.
        if (selection.is_none() && futures.is_empty()) || Instant::now() >= deadline {
            return Ok((results, WaitReturnReason::Timeout));
        }
        tokio::select! {
            biased;
            result = futures.next(), if !futures.is_empty() => {
                if let Some(Some(result)) = result {
                    results.push(result);
                }
            }
            changed = activity.changed(), if selection.is_some() => {
                changed.map_err(|err| FunctionCallError::RespondToModel(
                    format!("wait_agent mailbox activity closed: {err}")
                ))?;
                refresh_inventory = true;
            }
            _ = tokio::time::sleep_until(deadline) => {
                return Ok((results, WaitReturnReason::Timeout));
            }
        }
    }
}

#[cfg(test)]
#[path = "wait_mailbox_tests.rs"]
mod tests;
