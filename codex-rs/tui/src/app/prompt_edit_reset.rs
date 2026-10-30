//! Install authoritative prompt-edit history behind the existing transcript reset barrier.

use super::*;
use crate::app_server_session::HistoryHydrationScope;
use crate::chatwidget::UserMessage;
use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadRevertResponse;
use codex_app_server_protocol::Turn;
use color_eyre::eyre::eyre;
use std::collections::HashSet;

impl App {
    pub(super) async fn wait_for_prompt_revert(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
            while let Some(event) = app_server.next_event().await {
                let is_reverted = event.backend_id == app_server.backend_id()
                    && matches!(
                        &event.event,
                        AppServerEvent::ServerNotification(notification)
                            if matches!(notification.as_ref(), ServerNotification::ThreadReverted(notification)
                                if notification.thread_id == thread_id.to_string())
                    );
                self.handle_backend_event(app_server, event).await;
                if is_reverted {
                    return Ok(());
                }
            }
            Err(eyre!("app-server disconnected"))
        })
        .await
        .wrap_err(
            "history was reverted, but refreshing the session timed out; resume this session to reload it",
        )?
    }

    /// Model input can precede the visible steer, with concurrent tool output between them.
    /// Reload the retained window so removed intervening output cannot survive in the TUI.
    pub(super) async fn reconcile_steer_revert(
        &mut self,
        app_server: &mut AppServerSession,
        reverted: ThreadRevertResponse,
        mut retained_turns: Vec<Turn>,
        prompt: UserMessage,
    ) -> Result<()> {
        let thread_id = ThreadId::from_string(&reverted.thread.id)?;
        let anchor = retained_turns
            .iter()
            .find(|turn| !turn.items.is_empty())
            .or_else(|| retained_turns.last())
            .map(|turn| turn.id.clone())
            .ok_or_else(|| eyre!("retained steer turn disappeared"))?;
        let mut canonical = reverted.thread;
        tokio::time::timeout(
            Duration::from_secs(/*secs*/ 10),
            app_server.hydrate_initial_thread_history(
                &mut canonical,
                reverted.turns_backwards_cursor,
                reverted.items_backwards_cursor,
                // History-only hydration preserves the existing older-page cursors.
                /*config*/
                None,
                /*local_settings*/ None,
                HistoryHydrationScope::ThroughTurn(&anchor),
            ),
        )
        .await
        .wrap_err(
            "history changed but refresh timed out; reopen this session before continuing",
        )??;
        for retained in &mut retained_turns {
            if let Some(turn) = canonical.turns.iter().find(|turn| turn.id == retained.id) {
                let updated = {
                    let visible: HashSet<_> = retained.items.iter().map(ThreadItem::id).collect();
                    let mut updated = turn.clone();
                    updated.items.retain(|item| visible.contains(item.id()));
                    updated
                };
                *retained = updated;
            } else if !retained.items.is_empty() {
                color_eyre::eyre::bail!(
                    "history changed but a retained turn could not be refreshed; reopen this session before continuing"
                );
            }
        }
        canonical.turns = retained_turns;
        self.install_prompt_edit(thread_id, canonical, prompt).await
    }

    pub(super) async fn install_prompt_edit(
        &mut self,
        thread_id: ThreadId,
        canonical: Thread,
        prompt: UserMessage,
    ) -> Result<()> {
        let mut session = self
            .thread_event_channels
            .get(&thread_id)
            .ok_or_else(|| eyre!("edited thread is no longer available"))?
            .store
            .lock()
            .await
            .session
            .clone()
            .ok_or_else(|| eyre!("edited thread has no session"))?;
        self.abort_thread_event_listener(thread_id);
        // The history fence was consumed; discard already-routed events rather than replaying
        // stale item completions. Removing the store also invalidates its replay buffer.
        self.active_thread_rx = None;
        self.thread_event_channels.remove(&thread_id);
        self.active_thread_id = None;
        session.rollout_path = canonical.path.clone();
        if self.primary_thread_id == Some(thread_id) {
            self.primary_session_configured = Some(session.clone());
        }
        self.chat_widget.restore_thread_input_state(
            /*input_state*/ None,
            crate::chatwidget::ThreadInputStateRestoreMode {
                preserve_in_flight_turn: false,
            },
        );
        self.recap.reset_for_new_thread(Instant::now());
        self.recap.seed_from_turns(&canonical.turns, Instant::now());
        self.retain_realtime_replay_state_before_replace();
        self.forget_realtime_replay_thread(thread_id);
        self.chat_widget
            .reset_after_prompt_revert(canonical.path.clone(), &canonical.turns);
        self.chat_widget.restore_user_message_to_composer(prompt);
        self.chat_widget
            .set_queue_autosend_suppressed(/*suppressed*/ true);
        {
            let mut store = self.ensure_thread_channel(thread_id).store.lock().await;
            store.set_session(session, canonical.turns.clone());
            store.turn_history_complete = canonical.history_mode == ThreadHistoryMode::Legacy;
        }
        self.activate_thread_channel(thread_id).await;
        let visibility = if self.config.show_raw_agent_reasoning {
            crate::thread_transcript::RawReasoningVisibility::Visible
        } else {
            crate::thread_transcript::RawReasoningVisibility::Hidden
        };
        let mut config = self.config.clone();
        config.show_compact_summary = self.local_settings.tui.show_compact_summary;
        config.tui_agent_prompt_preview_lines = self.local_settings.tui.agent_prompt_preview_lines;
        config.tui_agent_response_preview_lines =
            self.local_settings.tui.agent_response_preview_lines;
        config.tui_command_output_preview_lines =
            self.local_settings.tui.command_output_preview_lines;
        config.tui_user_shell_output_preview_lines =
            self.local_settings.tui.user_shell_output_preview_lines;
        let mut cells = self
            .transcript_cells
            .iter()
            .rev()
            .find(|cell| cell.as_any().is::<crate::history_cell::SessionInfoCell>())
            .cloned()
            .into_iter()
            .collect::<Vec<_>>();
        cells.extend(crate::thread_transcript::thread_to_transcript_cells(
            canonical,
            visibility,
            Some(&config),
        ));
        // Widget shutdown can have queued transcript inserts. Install the authoritative
        // replacement after them, using the same input/reset barrier as Paginated revert.
        self.app_event_tx.send(AppEvent::FinishPromptRevert {
            thread_id,
            nth_user_message: 0,
            canonical_cells: Some(cells),
        });
        Ok(())
    }
}
