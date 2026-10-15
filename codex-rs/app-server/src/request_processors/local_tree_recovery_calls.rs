//! Validate canonical tool-call identities before recovering an unfinished turn.
//!
//! Missing outcomes use ordinary prompt-only placeholders when sampling resumes.
//! Those placeholders do not certify completion or directly redispatch saved calls.

use codex_protocol::models::ResponseItem;
use codex_rollout::RolloutItem;
use std::collections::HashSet;
use std::io;

pub(super) fn validate_call_identities(items: &[RolloutItem]) -> io::Result<()> {
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
    Ok(())
}
