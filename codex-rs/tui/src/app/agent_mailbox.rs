//! Read-only mailbox counts shown for the selected `/agent` receiver.

use ratatui::style::Stylize;
use ratatui::text::Line;
use uuid::Uuid;

use codex_app_server_protocol::AgentAliasState;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadMailboxPendingSender;
use codex_app_server_protocol::ThreadMailboxReadParams;
use codex_app_server_protocol::ThreadMailboxReadResponse;

use super::AgentControlPanePreview;
use super::App;
use super::agent_navigation::AgentNavigationState;
use crate::app_event::AppEvent;
use crate::app_server_session::AppServerSession;

#[derive(Clone, Debug, Default)]
pub(super) enum AgentMailboxDetails {
    #[default]
    NotShown,
    Loading,
    Unavailable,
    Inventory {
        pending_total: u64,
        sender_counts: Vec<(String, u64)>,
    },
}

impl AgentMailboxDetails {
    pub(super) fn lines(&self) -> Vec<Line<'static>> {
        match self {
            Self::NotShown => Vec::new(),
            Self::Loading => vec![vec!["Pending mail: ".bold(), "loading".dim()].into()],
            Self::Unavailable => vec![vec!["Pending mail: ".bold(), "unavailable".dim()].into()],
            Self::Inventory {
                pending_total,
                sender_counts,
            } => {
                let mut lines =
                    vec![vec!["Pending mail: ".bold(), pending_total.to_string().into()].into()];
                lines.extend(sender_counts.iter().map(|(sender, count)| {
                    vec![
                        "  ".dim(),
                        sender.clone().into(),
                        ": ".dim(),
                        count.to_string().into(),
                    ]
                    .into()
                }));
                lines
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum AgentMailboxInventoryDisplay {
    Unavailable,
    Available {
        pending_total: u64,
        sender_counts: Vec<(String, u64)>,
    },
}

fn mailbox_inventory_display(
    receiver_thread_id: codex_protocol::ThreadId,
    navigation: &AgentNavigationState,
    result: Result<ThreadMailboxReadResponse, String>,
) -> AgentMailboxInventoryDisplay {
    match result {
        Ok(inventory) => {
            let sender_counts = inventory
                .pending_senders
                .into_iter()
                .map(|sender| match sender {
                    ThreadMailboxPendingSender::User { count } => ("user".to_string(), count),
                    ThreadMailboxPendingSender::Agent { thread_id, count } => {
                        match codex_protocol::ThreadId::from_string(&thread_id) {
                            Ok(sender_id) => {
                                let label = navigation
                                    .alias(sender_id)
                                    .filter(|alias| alias.state != AgentAliasState::Transferred)
                                    .map(|alias| {
                                        let agent_ref = alias.agent_ref;
                                        format!("ref {agent_ref}")
                                    })
                                    .unwrap_or_else(|| sender_id.to_string());
                                (label, count)
                            }
                            Err(_) => (thread_id, count),
                        }
                    }
                })
                .collect();
            AgentMailboxInventoryDisplay::Available {
                pending_total: inventory.pending_total,
                sender_counts,
            }
        }
        Err(error) => {
            tracing::warn!(
                %error,
                %receiver_thread_id,
                "failed to read selected agent mailbox inventory"
            );
            AgentMailboxInventoryDisplay::Unavailable
        }
    }
}

impl App {
    pub(super) fn request_agent_mailbox_inventory(
        &self,
        app_server: &AppServerSession,
        receiver_thread_id: codex_protocol::ThreadId,
        request_id: Uuid,
        preview: AgentControlPanePreview,
    ) {
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = request_handle
                .request_typed::<ThreadMailboxReadResponse>(ClientRequest::ThreadMailboxRead {
                    request_id: RequestId::String(Uuid::new_v4().to_string()),
                    params: ThreadMailboxReadParams {
                        thread_id: receiver_thread_id.to_string(),
                    },
                })
                .await
                .map_err(|error| error.to_string());
            app_event_tx.send(AppEvent::AgentMailboxInventoryLoaded {
                receiver_thread_id,
                request_id,
                preview,
                result,
            });
        });
    }

    pub(super) fn apply_agent_mailbox_inventory(
        &mut self,
        receiver_thread_id: codex_protocol::ThreadId,
        request_id: Uuid,
        preview: AgentControlPanePreview,
        result: Result<ThreadMailboxReadResponse, String>,
    ) {
        let display = mailbox_inventory_display(receiver_thread_id, &self.agent_navigation, result);
        if preview.finish_mailbox_read(receiver_thread_id, request_id, display) {
            self.chat_widget.request_redraw();
        }
    }
}

#[cfg(test)]
#[path = "agent_mailbox_tests.rs"]
mod tests;
