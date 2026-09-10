//! Keyboard switching only considers available members of the current agent root.

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AgentThreadSelectionMode {
    Inspect,
    AvailableOnly,
}

impl App {
    pub(super) fn cycle_attachment_available(&self, thread_id: ThreadId) -> bool {
        // An absent channel is undiscovered locally, not necessarily unloaded on the server.
        self.thread_event_channels
            .get(&thread_id)
            .is_none_or(|channel| channel.attachment() == ThreadEventAttachment::Live)
    }

    pub(super) async fn cycle_available_agent(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        direction: AgentNavigationDirection,
    ) -> Result<()> {
        let mut attempted = HashSet::new();
        let mut candidate = self
            .adjacent_thread_id_with_backfill(app_server, direction, &attempted)
            .await;
        // Bound the entire keypress even if discovery fails or the server graph changes.
        let limit = self.agent_navigation.tracked_thread_ids().len();
        for _ in 0..limit {
            let Some(thread_id) = candidate else {
                break;
            };
            attempted.insert(thread_id);
            let side_to_discard = self.side_thread_to_discard_after_switch(thread_id);
            if self
                .select_agent_thread_with_mode(
                    tui,
                    app_server,
                    thread_id,
                    AgentThreadSelectionMode::AvailableOnly,
                )
                .await?
            {
                if let Some(side_thread_id) = side_to_discard {
                    self.discard_side_thread_in_background(app_server, side_thread_id)
                        .await;
                    self.surface_pending_inactive_thread_interactive_requests()
                        .await?;
                }
                break;
            }
            candidate = self
                .adjacent_thread_id_with_backfill(app_server, direction, &attempted)
                .await;
        }
        Ok(())
    }

    /// A metadata-only row can be server-loaded without a local subscription. Read its current
    /// status before the existing attach path; never attach a known unloaded or failed target.
    pub(super) async fn refresh_available_agent_for_cycle(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) -> bool {
        let Some(root) = self.agent_root_thread_id() else {
            return false;
        };
        if !self.agent_navigation.is_cycle_member(root, thread_id)
            || !self.cycle_attachment_available(thread_id)
        {
            return false;
        }
        let Ok(thread) = app_server
            .thread_read(thread_id, /*include_turns*/ false)
            .await
        else {
            return false;
        };
        self.agent_root_thread_id() == Some(root)
            && self.agent_navigation.is_cycle_member(root, thread_id)
            && thread.session_id == root.to_string()
            && matches!(
                thread.status,
                codex_app_server_protocol::ThreadStatus::Idle
                    | codex_app_server_protocol::ThreadStatus::Active { .. }
            )
    }
}
