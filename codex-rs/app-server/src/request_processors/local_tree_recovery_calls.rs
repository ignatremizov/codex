//! Crash recovery must not mistake a missing tool result for a failed operation.
//!
//! Inspect canonical records before prompt normalization can supply synthetic `aborted`
//! outputs. Process death does not prove that an external operation had no effects.

use codex_protocol::models::ResponseItem;
use codex_rollout::RolloutItem;
use std::collections::HashSet;
use std::io;

pub(super) fn validate_call_outcomes(items: &[RolloutItem]) -> io::Result<()> {
    let mut functions = HashSet::new();
    let mut custom = HashSet::new();
    for item in items {
        let RolloutItem::ResponseItem(envelope) = item else {
            continue;
        };
        match &envelope.item {
            ResponseItem::FunctionCall { call_id, .. }
            | ResponseItem::LocalShellCall {
                call_id: Some(call_id),
                ..
            } => {
                if call_id.is_empty() || !functions.insert(call_id.as_str()) {
                    return Err(io::Error::other(
                        "ambiguous tool call identity requires explicit reconciliation",
                    ));
                }
            }
            ResponseItem::CustomToolCall { call_id, .. } => {
                if call_id.is_empty() || !custom.insert(call_id.as_str()) {
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
            ResponseItem::LocalShellCall { call_id: None, .. } => {
                return Err(io::Error::other(
                    "unidentified shell call requires explicit reconciliation",
                ));
            }
            _ => {}
        }
    }
    if !functions.is_empty() || !custom.is_empty() {
        return Err(io::Error::other(
            "tool call outcome was not saved; reconcile its effects before explicit retry",
        ));
    }
    Ok(())
}
