//! Eligibility for continuing durable receiver-owned turns.
//!
//! Callers must supply canonical persisted records to establish ownership and lifecycle.
//! Missing tool outcomes are left to ordinary prompt normalization when sampling resumes.

use crate::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnContextItem;
use std::collections::HashSet;
use std::io;

/// Which durable terminal states permit continuing the most recent own turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRecoveryMode {
    /// Recover unfinished work following local server death.
    Automatic,
    /// Also permit a user-interrupted regular turn.
    Explicit,
}

/// Select the latest receiver-owned regular turn.
///
/// Repeated starts retain original input and call history, while only the lifecycle
/// after the latest start determines whether that logical turn is unfinished.
/// Ownership, settings, or call-identity ambiguity is an error. Missing tool outcomes
/// permit continuation without directly redispatching the recorded calls.
pub fn select_recoverable_turn(
    thread_id: ThreadId,
    segment: &[RolloutItem],
    mode: TurnRecoveryMode,
) -> io::Result<Option<TurnContextItem>> {
    let meta = segment
        .iter()
        .find_map(|item| match item {
            RolloutItem::SessionMeta(meta) => Some(&meta.meta),
            _ => None,
        })
        .ok_or_else(|| io::Error::other("canonical history lacks ownership metadata"))?;
    if meta.id != thread_id {
        return Err(io::Error::other(
            "canonical history belongs to another thread",
        ));
    }
    let own_start = match meta.subagent_history_start_ordinal {
        Some(ordinal) => usize::try_from(
            ordinal.saturating_sub(
                meta.history_base
                    .map_or(0, |base| base.end_ordinal_exclusive),
            ),
        )
        .map_err(io::Error::other)?,
        None if meta.parent_thread_id.is_some() || meta.forked_from_id.is_some() => segment
            .iter()
            .position(|item| {
                matches!(item,
                        RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(settings))
                            if settings.thread_id == Some(thread_id))
            })
            .ok_or_else(|| {
                io::Error::other("inherited history has no trustworthy own-turn boundary")
            })?,
        None => 0,
    };
    let own = segment
        .get(own_start..)
        .ok_or_else(|| io::Error::other("invalid own-turn boundary"))?;
    let Some(mut start) = own
        .iter()
        .rposition(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(_))))
    else {
        return Ok(None);
    };
    let RolloutItem::EventMsg(EventMsg::TurnStarted(started)) = &own[start] else {
        unreachable!("turn start selected above");
    };
    let latest_start = start;
    // Recovery records another start for the same logical turn, without another user message.
    // Retain that turn's original durable input, but never cross a different turn's boundary.
    while let Some(previous) = own[..start]
        .iter()
        .rposition(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(_))))
    {
        if !matches!(&own[previous], RolloutItem::EventMsg(EventMsg::TurnStarted(event))
            if event.turn_id == started.turn_id)
        {
            break;
        }
        start = previous;
    }
    let tail = &own[start..];
    if own[latest_start..].iter().any(|item| match item {
        RolloutItem::EventMsg(EventMsg::TurnComplete(event)) => event.turn_id == started.turn_id,
        RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => {
            event
                .turn_id
                .as_ref()
                .is_none_or(|id| id == &started.turn_id)
                && (mode == TurnRecoveryMode::Automatic
                    || event.reason != TurnAbortReason::Interrupted
                    || event.error.is_some())
        }
        RolloutItem::EventMsg(EventMsg::Error(event)) => event.affects_turn_status(),
        _ => false,
    }) {
        return Ok(None);
    }
    if tail.iter().any(|item| {
        matches!(
            item,
            RolloutItem::EventMsg(EventMsg::EnteredReviewMode(_))
                | RolloutItem::EventMsg(EventMsg::ExitedReviewMode(_))
                | RolloutItem::EventMsg(EventMsg::ThreadRolledBack(_))
                | RolloutItem::RealtimeItem(_)
        )
    }) {
        return Err(io::Error::other(
            "non-regular or rolled-back work requires explicit recovery",
        ));
    }
    let context = tail
        .iter()
        .rev()
        .find_map(|item| match item {
            RolloutItem::TurnContext(context)
                if context.turn_id.as_ref() == Some(&started.turn_id) =>
            {
                Some(context.clone())
            }
            _ => None,
        })
        .ok_or_else(|| io::Error::other("unfinished turn lacks its original settings"))?;
    if context.recovery_start_options.is_none() {
        return Err(io::Error::other(
            "original turn start options were not recorded; explicit retry is required",
        ));
    }
    let recipient = meta.agent_path.as_deref().or_else(|| {
        meta.parent_thread_id
            .is_none()
            .then_some(codex_protocol::AgentPath::ROOT)
    });
    let has_input = tail.iter().any(|item| match item {
        RolloutItem::ResponseItem(envelope)
            if !envelope
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.inherited_user_message) =>
        {
            match &envelope.item {
                ResponseItem::Message { role, content, .. } => {
                    role == "user" && !content.is_empty()
                }
                ResponseItem::AgentMessage {
                    recipient: target,
                    content,
                    ..
                } => recipient == Some(target.as_str()) && !content.is_empty(),
                _ => false,
            }
        }
        RolloutItem::InterAgentCommunication(message) => {
            recipient == Some(message.recipient.as_str())
                && (!message.content.is_empty() || message.encrypted_content.is_some())
        }
        _ => false,
    });
    if context.realtime_active == Some(true) || !has_input {
        return Err(io::Error::other(
            "unfinished turn lacks durable regular input",
        ));
    }
    validate_call_identities(tail)?;
    Ok(Some(context))
}

fn validate_call_identities(items: &[RolloutItem]) -> io::Result<()> {
    let mut functions = HashSet::new();
    let mut custom = HashSet::new();
    let mut searches = HashSet::new();
    for item in items {
        let RolloutItem::ResponseItem(envelope) = item else {
            continue;
        };
        match &envelope.item {
            ResponseItem::FunctionCall { call_id, .. }
            | ResponseItem::LocalShellCall {
                call_id: Some(call_id),
                ..
            } if call_id.is_empty() || !functions.insert(call_id.as_str()) => {
                return Err(io::Error::other(
                    "ambiguous tool call identity requires explicit reconciliation",
                ));
            }
            ResponseItem::CustomToolCall { call_id, .. }
                if call_id.is_empty() || !custom.insert(call_id.as_str()) =>
            {
                return Err(io::Error::other(
                    "ambiguous tool call identity requires explicit reconciliation",
                ));
            }
            ResponseItem::ToolSearchCall {
                call_id, execution, ..
            } if execution == "client" => {
                let Some(call_id) = call_id.as_deref().filter(|id| !id.is_empty()) else {
                    return Err(io::Error::other(
                        "unidentified tool search requires explicit reconciliation",
                    ));
                };
                if !searches.insert(call_id) {
                    return Err(io::Error::other(
                        "ambiguous tool call identity requires explicit reconciliation",
                    ));
                }
            }
            ResponseItem::FunctionCallOutput {
                call_id: Some(call_id),
                ..
            } => {
                functions.remove(call_id.as_str());
            }
            ResponseItem::CustomToolCallOutput { call_id, .. } => {
                custom.remove(call_id.as_str());
            }
            ResponseItem::ToolSearchOutput {
                call_id: Some(call_id),
                execution,
                ..
            } if execution == "client" => {
                searches.remove(call_id.as_str());
            }
            ResponseItem::LocalShellCall { call_id: None, .. } => {
                return Err(io::Error::other(
                    "unidentified shell call requires explicit reconciliation",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "turn_recovery_tests.rs"]
mod tests;
