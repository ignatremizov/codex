//! Thread-scoped MCP inventory and explicit activation results.
//!
//! Inventory sequence numbers are independent of other background work and never reused on reset.
//! Buffered results keep their original scope so switching conversations cannot redirect output.

use super::*;
use codex_app_server_protocol::McpServerStatus;
use codex_app_server_protocol::McpServerStatusDetail;
use codex_app_server_protocol::ThreadMcpServerActivateOutcome;

#[derive(Default)]
pub(super) struct McpRequests {
    next_sequence: u64,
    latest: HashMap<Option<ThreadId>, (u64, bool)>,
}

impl McpRequests {
    pub(super) fn start(&mut self, thread_id: Option<ThreadId>) -> u64 {
        self.next_sequence += 1;
        self.latest.insert(thread_id, (self.next_sequence, true));
        self.next_sequence
    }

    pub(super) fn reset(&mut self) {
        self.latest.clear();
    }

    fn is_current(&self, scope: Option<ThreadId>, sequence: u64) -> bool {
        self.latest
            .get(&scope)
            .is_some_and(|(latest, _)| *latest == sequence)
    }
}

#[derive(Clone, Debug)]
pub(super) enum McpThreadEvent {
    Inventory {
        scope: Option<ThreadId>,
        sequence: u64,
        result: Result<Vec<McpServerStatus>, String>,
        detail: McpServerStatusDetail,
    },
    Activation {
        server_name: String,
        result: Result<ThreadMcpServerActivateOutcome, String>,
    },
}

impl App {
    pub(super) async fn handle_mcp_inventory_result(
        &mut self,
        result: Result<Vec<McpServerStatus>, String>,
        detail: McpServerStatusDetail,
        thread_id: Option<ThreadId>,
        sequence: u64,
    ) {
        if !self.mcp_requests.is_current(thread_id, sequence) {
            return;
        }
        self.mcp_requests
            .latest
            .insert(thread_id, (sequence, false));
        let event = McpThreadEvent::Inventory {
            scope: thread_id,
            sequence,
            result,
            detail,
        };
        self.enqueue_mcp_result(thread_id.or(self.primary_thread_id), event)
            .await;
    }

    pub(super) async fn enqueue_mcp_result(
        &mut self,
        thread_id: Option<ThreadId>,
        event: McpThreadEvent,
    ) {
        let Some(thread_id) = thread_id else {
            if self.current_displayed_thread_id().is_none() {
                self.render_mcp_result(event);
            } else {
                self.pending_primary_events
                    .push_back(ThreadBufferedEvent::Mcp(event));
            }
            return;
        };
        let (sender, store) = {
            let channel = self.ensure_thread_channel(thread_id);
            (channel.sender.clone(), Arc::clone(&channel.store))
        };
        let event = ThreadBufferedEvent::Mcp(event);
        let active = {
            let mut store = store.lock().await;
            store.push_buffered_event(event.clone());
            store.active
        };
        if active {
            match sender.try_send(event) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(event)) => {
                    tokio::spawn(async move {
                        let _ = sender.send(event).await;
                    });
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
    }

    pub(super) fn render_mcp_result(&mut self, event: McpThreadEvent) {
        match event {
            McpThreadEvent::Inventory {
                scope,
                sequence,
                result,
                detail,
            } => {
                if !self.mcp_requests.is_current(scope, sequence) {
                    return;
                }
                self.chat_widget.clear_mcp_inventory_loading();
                self.clear_committed_mcp_inventory_loading();
                match result {
                    Ok(statuses) => {
                        self.chat_widget.sync_mcp_server_completions(&statuses);
                        if statuses.is_empty() {
                            self.chat_widget
                                .add_to_history(history_cell::empty_mcp_output());
                        } else {
                            self.chat_widget.add_to_history(
                                history_cell::new_mcp_tools_output_from_statuses(&statuses, detail),
                            );
                        }
                    }
                    Err(error) => self
                        .chat_widget
                        .add_error_message(format!("Failed to load MCP inventory: {error}")),
                }
            }
            McpThreadEvent::Activation {
                server_name,
                result,
            } => {
                let message = match result {
                    Ok(ThreadMcpServerActivateOutcome::Activated) => {
                        format!("MCP server `{server_name}` activation requested.")
                    }
                    Ok(ThreadMcpServerActivateOutcome::AlreadyActivated) => {
                        format!("MCP server `{server_name}` tools are already in context.")
                    }
                    Ok(ThreadMcpServerActivateOutcome::AlreadyImplicitlyAvailable) => format!(
                        "MCP server `{server_name}` is already visible to the model by default."
                    ),
                    Err(error) => {
                        self.chat_widget.add_error_message(format!(
                            "Failed to use MCP server `{server_name}`: {error}"
                        ));
                        return;
                    }
                };
                self.chat_widget.add_info_message(message, /*hint*/ None);
            }
        }
    }

    pub(super) fn sync_mcp_inventory_loading(&mut self) {
        if !self
            .mcp_requests
            .latest
            .get(&self.current_displayed_thread_id())
            .is_some_and(|(_, pending)| *pending)
        {
            self.chat_widget.clear_mcp_inventory_loading();
            self.clear_committed_mcp_inventory_loading();
        }
    }
}

#[cfg(test)]
#[path = "mcp_requests_tests.rs"]
mod tests;
