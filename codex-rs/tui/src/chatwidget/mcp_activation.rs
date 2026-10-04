//! Explicit MCP requests and server-name completion belong to the displayed conversation.
//! Pre-session requests remain FIFO and are admitted before the first user turn.

use super::*;
use codex_app_server_protocol::McpServerStatus;
use std::collections::BTreeSet;
use std::collections::VecDeque;

#[derive(Default)]
pub(super) struct McpActivation {
    pending: VecDeque<String>,
    known_names: HashMap<Option<ThreadId>, BTreeSet<String>>,
}

impl ChatWidget {
    pub(super) fn handle_mcp_args(&mut self, args: &str) {
        if args.eq_ignore_ascii_case("verbose") {
            self.add_mcp_output(McpServerStatusDetail::Full);
            return;
        }
        let Some((verb, server_name)) = args.split_once(char::is_whitespace) else {
            self.add_error_message("Usage: /mcp [verbose] | /mcp use <server>".into());
            return;
        };
        let server_name = server_name.trim();
        if !verb.eq_ignore_ascii_case("use") || server_name.is_empty() {
            self.add_error_message("Usage: /mcp [verbose] | /mcp use <server>".into());
            return;
        }
        let op = AppCommand::ActivateMcpServer {
            server_name: server_name.to_string(),
        };
        if self.external_writer_view {
            self.add_error_message(
                "This thread is open elsewhere. Close it there and retry resume to continue."
                    .into(),
            );
            return;
        }
        if self.rejects_misalignment_policy_op(&op) {
            return;
        }
        if let Some(thread_id) = self.thread_id {
            self.app_event_tx
                .send(AppEvent::SubmitThreadOp { thread_id, op });
        } else if !self
            .mcp_activation
            .pending
            .iter()
            .any(|name| name == server_name)
        {
            self.mcp_activation
                .pending
                .push_back(server_name.to_string());
            self.add_info_message(
                format!("MCP server `{server_name}` activation will be requested when the session starts."),
                /*hint*/ None,
            );
        }
    }

    pub(super) fn bind_mcp_activation(&mut self) {
        self.refresh_mcp_server_completions();
        if let Some(thread_id) = self.thread_id
            && !self.external_writer_view
        {
            while let Some(server_name) = self.mcp_activation.pending.pop_front() {
                self.app_event_tx.send(AppEvent::SubmitThreadOp {
                    thread_id,
                    op: AppCommand::ActivateMcpServer { server_name },
                });
            }
        }
    }

    pub(crate) fn sync_mcp_server_completions(&mut self, statuses: &[McpServerStatus]) {
        self.mcp_activation.known_names.insert(
            self.thread_id,
            statuses.iter().map(|status| status.name.clone()).collect(),
        );
        self.refresh_mcp_server_completions();
    }

    pub(super) fn note_mcp_server_name(&mut self, name: String) {
        self.mcp_activation
            .known_names
            .entry(self.thread_id)
            .or_default()
            .insert(name);
        self.refresh_mcp_server_completions();
    }

    fn refresh_mcp_server_completions(&mut self) {
        let mut names: BTreeSet<String> = self.config.mcp_servers.get().keys().cloned().collect();
        if let Some(known) = self.mcp_activation.known_names.get(&self.thread_id) {
            names.extend(known.iter().cloned());
        }
        self.bottom_pane
            .set_mcp_server_names(names.into_iter().collect());
    }
}
