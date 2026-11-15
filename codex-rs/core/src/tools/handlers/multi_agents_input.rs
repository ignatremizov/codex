//! Shared target syntax and compact tool receipts for agent-input adapters.
//!
//! Logging retains admission identities. Both direct and Code Mode model results use the same
//! small projection; an accepted receipt is not proof that the receiver read or completed input.

use super::multi_agents_common::tool_output_code_mode_result;
use super::multi_agents_common::tool_output_json_text;
use super::multi_agents_common::tool_output_response_item;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use codex_protocol::models::ResponseInputItem;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum SendInputTarget {
    Single(String),
    Batch(Vec<String>),
}

#[derive(Debug, Serialize)]
pub(crate) struct SendInputResult {
    pub(crate) submission_id: String,
    pub(crate) status: SendInputAdmissionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) hint: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum SendInputAdmissionStatus {
    Submitted,
    Queued,
    MailboxAccepted,
}

#[derive(Serialize)]
struct SendInputModelResult<'a> {
    status: &'a SendInputAdmissionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'a str>,
}

impl ToolOutput for SendInputResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "send_input")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(
            call_id,
            payload,
            &SendInputModelResult {
                status: &self.status,
                hint: self.hint.as_deref(),
            },
            Some(true),
            "send_input",
        )
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(
            &SendInputModelResult {
                status: &self.status,
                hint: self.hint.as_deref(),
            },
            "send_input",
        )
    }
}

#[cfg(test)]
#[path = "multi_agents_input_tests.rs"]
mod tests;
