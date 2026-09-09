//! Receiver-owned durable aliases frozen for one model request.

use std::collections::HashMap;

use codex_agent_graph_store::AgentAlias;
use codex_agent_graph_store::AgentAliasState;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::error::Result as CodexResult;
use codex_utils_string::approx_bytes_for_tokens;
use serde_json::json;

use super::AgentControl;

// Discovery context only: never truncate instructions, payloads, or alias labels.
const IDENTITY_SUMMARY_TOKENS: usize = 2_048;
const IDENTITY_INSTRUCTIONS: &str = "This is the current receiving root's agent identity mapping and replaces earlier mappings. \
     Refs identify only the agents listed here; use a canonical agent_id for other agents. \
     Task paths and nicknames are descriptive and may change.";
const OMITTED_IDENTITIES: &str =
    " Additional identities are omitted; use the existing agent directory/discovery for details.";

/// One alias namespace shared by identity hydration and disposable envelope projection.
#[derive(Clone, Debug, Default)]
pub(crate) struct V1AgentIdentitySnapshot {
    pub(crate) refs: HashMap<ThreadId, u64>,
    entries: Vec<AgentAlias>,
    omitted: bool,
}

impl V1AgentIdentitySnapshot {
    fn from_aliases(session_id: SessionId, aliases: Vec<AgentAlias>) -> Self {
        let mut entries = aliases
            .into_iter()
            .filter(|alias| {
                alias.session_id == session_id
                    && matches!(
                        alias.state,
                        AgentAliasState::Active | AgentAliasState::Closed
                    )
            })
            .collect::<Vec<_>>();
        // A root-only namespace needs no cross-agent mapping. Clear both views
        // together so projection cannot emit a ref without its hydration.
        if entries
            .iter()
            .all(|alias| SessionId::from(alias.thread_id) == session_id)
        {
            return Self::default();
        }
        // Refs are monotonic within a namespace. Prefer active members, then newer
        // closed refs; the store does not expose closure timestamps in this query.
        entries.sort_by_key(|alias| match alias.state {
            AgentAliasState::Active => (0, alias.agent_ref),
            AgentAliasState::Closed => (1, u64::MAX - alias.agent_ref),
            AgentAliasState::Transferred => (2, alias.agent_ref),
        });
        // Reserve both markers, their newlines, the instructions-to-JSON newline,
        // array brackets, and the worst-case omission notice before selecting rows.
        let mut remaining = approx_bytes_for_tokens(IDENTITY_SUMMARY_TOKENS).saturating_sub(
            IDENTITY_INSTRUCTIONS.len()
                + OMITTED_IDENTITIES.len()
                + "<agent_identity_context>\n\n</agent_identity_context>".len()
                + 3,
        );
        let mut omitted = false;
        entries.retain(|alias| {
            let bytes = json!({
                "ref": alias.agent_ref.to_string(),
                "nickname": alias.nickname,
                "task_path": alias.task_path,
                "state": alias.state,
            })
            .to_string()
            .len()
                + 1;
            if bytes > remaining {
                omitted = true;
                false
            } else {
                remaining -= bytes;
                true
            }
        });
        let refs = entries
            .iter()
            .map(|alias| (alias.thread_id, alias.agent_ref))
            .collect();
        Self {
            refs,
            entries,
            omitted,
        }
    }

    pub(crate) fn hydration_body(&self) -> String {
        if self.entries.is_empty() && !self.omitted {
            return String::new();
        }
        let identities = self
            .entries
            .iter()
            .map(|alias| {
                json!({
                    "ref": alias.agent_ref.to_string(),
                    "nickname": alias.nickname,
                    "task_path": alias.task_path,
                    "state": alias.state,
                })
            })
            .collect::<Vec<_>>();
        let omission = if self.omitted { OMITTED_IDENTITIES } else { "" };
        format!(
            "{IDENTITY_INSTRUCTIONS}{omission}\n{}",
            serde_json::Value::Array(identities)
        )
    }
}

impl AgentControl {
    pub(crate) async fn v1_agent_identity_snapshot(&self) -> CodexResult<V1AgentIdentitySnapshot> {
        let Some(session_id) = self.bound_session_id() else {
            return Ok(V1AgentIdentitySnapshot::default());
        };
        // Standalone sessions have no manager/alias authority. Keep that supported
        // no-op distinct from a managed store failing to read its namespace.
        let Some(_manager) = self.manager.upgrade() else {
            return Ok(V1AgentIdentitySnapshot::default());
        };
        let aliases = self.list_session_agent_aliases().await?;
        Ok(V1AgentIdentitySnapshot::from_aliases(session_id, aliases))
    }
}

#[cfg(test)]
#[path = "identity_snapshot_tests.rs"]
mod tests;
