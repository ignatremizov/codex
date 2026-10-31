//! Detach idle branches and report ownership changes behind the old runtime's closure fence.

use super::*;
use crate::app_server_session::AppServerSession;
use crate::chatwidget::agent_command::AgentSelector;
use crate::resume_picker::SessionTarget;
use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol::AgentAliasState;
use codex_app_server_protocol::CollabAgentLifecycleAction;
use codex_app_server_protocol::ThreadDetachResponse;
use codex_app_server_protocol::ThreadStatus;
use std::collections::HashSet;

impl App {
    pub(super) async fn detach_agent_from_command(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        source: ThreadId,
        selector: Option<AgentSelector>,
    ) -> Result<()> {
        if self.chat_widget.thread_id() != Some(source) {
            return Ok(());
        }
        let target = match selector {
            Some(selector) => match self
                .validated_agent_control_target(app_server, source, &selector)
                .await
            {
                Ok(target) => ThreadId::from_string(&target)?,
                Err(error) => {
                    self.chat_widget.add_error_message(error);
                    return Ok(());
                }
            },
            None => source,
        };
        if target == source {
            return self.detach_displayed_agent(tui, app_server, source).await;
        }
        if self.windows_sandbox_blocks_thread_switch()
            || self.reject_pending_permission_root_switch()
        {
            self.chat_widget.add_error_message(
                "Finish the current permission or sandbox setup before detaching.".into(),
            );
            return Ok(());
        }
        // A targeted command must leave the displayed conversation outside the moved branch.
        let mut ancestor = source;
        for _ in 0..self.agent_navigation.ordered_threads().len() {
            let Some(parent) = self.agent_navigation.parent_thread_id(ancestor) else {
                break;
            };
            if parent == target {
                self.chat_widget.add_error_message(
                    "Switch to that conversation before detaching its branch.".into(),
                );
                return Ok(());
            }
            ancestor = parent;
        }
        let label = self.thread_label(target);
        let detached = match app_server.thread_detach(target).await {
            Ok(detached) => detached,
            Err(error) => {
                self.chat_widget.add_error_message(format!(
                    "Could not detach {label}: {error:#}. Check its current ownership before retrying."
                ));
                return Ok(());
            }
        };
        if detached
            .unloaded_thread_ids
            .iter()
            .any(|id| *id == source.to_string())
        {
            // Retain this window if ownership metadata changed after the preflight.
            self.history_recovery_required.insert(source);
            self.abort_thread_event_listener(source);
            self.active_thread_rx = None;
            self.active_thread_id = None;
            self.chat_widget.pause_unavailable_thread();
        }
        if let Err(error) = self
            .await_detached_agent_closures(app_server, &detached)
            .await
        {
            self.chat_widget.add_error_message(format!(
                "{label} detached, but refreshing its state failed: {error:#}. Resume {target} in another TUI to continue."
            ));
            return Ok(());
        }
        self.refresh_primary_agent_aliases(app_server).await;
        self.agent_navigation
            .set_parent_thread_id(target, /*parent_thread_id*/ None);
        self.agent_navigation.announced_detachments.insert(target);
        self.sync_active_agent_label();
        self.chat_widget.add_info_message(
            format!("Detached {label}. Resume {target} in another TUI to continue independently."),
            /*hint*/ None,
        );
        Ok(())
    }

    /// A closed actor alone is not proof of detachment; confirm the durable independent root.
    pub(super) async fn track_agent_detachments(
        &mut self,
        app_server: &AppServerSession,
        notification: &ServerNotification,
    ) {
        let completed_call = match notification {
            ServerNotification::ItemStarted(started) => {
                if let ThreadItem::CollabAgentToolCall {
                    id,
                    lifecycle: Some(lifecycle),
                    tool: codex_app_server_protocol::CollabAgentTool::CloseAgent,
                    receiver_thread_ids,
                    ..
                } = &started.item
                    && lifecycle.action == CollabAgentLifecycleAction::Detach
                {
                    self.agent_navigation.pending_detachment_calls.insert(
                        (
                            started.thread_id.clone(),
                            started.turn_id.clone(),
                            id.clone(),
                        ),
                        receiver_thread_ids
                            .iter()
                            .filter_map(|id| ThreadId::from_string(id).ok())
                            .collect(),
                    );
                }
                None
            }
            ServerNotification::ItemCompleted(completed) => {
                let key = (
                    completed.thread_id.clone(),
                    completed.turn_id.clone(),
                    completed.item.id().to_string(),
                );
                if let ThreadItem::CollabAgentToolCall {
                    lifecycle: Some(lifecycle),
                    tool: codex_app_server_protocol::CollabAgentTool::CloseAgent,
                    receiver_thread_ids,
                    ..
                } = &completed.item
                    && lifecycle.action == CollabAgentLifecycleAction::Detach
                {
                    self.agent_navigation
                        .pending_detachment_calls
                        .entry(key.clone())
                        .or_insert_with(|| {
                            receiver_thread_ids
                                .iter()
                                .filter_map(|id| ThreadId::from_string(id).ok())
                                .collect()
                        });
                }
                Some(key)
            }
            ServerNotification::TurnCompleted(completed) => {
                self.agent_navigation.pending_detachment_calls.retain(
                    |(thread_id, turn_id, _), _| {
                        *thread_id != completed.thread_id || *turn_id != completed.turn.id
                    },
                );
                None
            }
            _ => None,
        };
        let candidates = match notification {
            ServerNotification::ThreadClosed(closed) => vec![closed.thread_id.clone()],
            // Model detachment can close the actor before committing ownership. The accepted
            // close result is a second boundary at which the durable root is settled.
            ServerNotification::ItemCompleted(completed) => match &completed.item {
                ThreadItem::CollabAgentToolCall {
                    tool: codex_app_server_protocol::CollabAgentTool::CloseAgent,
                    status: codex_app_server_protocol::CollabAgentToolCallStatus::Completed,
                    receiver_thread_ids,
                    ..
                } => receiver_thread_ids.clone(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        for candidate in candidates {
            if let Ok(thread_id) = ThreadId::from_string(&candidate) {
                self.surface_agent_detachment(app_server, thread_id).await;
            }
        }
        if let Some(key) = completed_call {
            self.agent_navigation.pending_detachment_calls.remove(&key);
        }
    }

    async fn surface_agent_detachment(
        &mut self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
    ) {
        let Some(root) = self.agent_root_thread_id() else {
            return;
        };
        if thread_id == root
            || self.current_displayed_thread_id() == Some(thread_id)
            || !self.owns_thread_for_routing(thread_id)
            || (self.agent_navigation.parent_thread_id(thread_id).is_none()
                && self
                    .agent_navigation
                    .alias(thread_id)
                    .is_none_or(|alias| alias.agent_ref == 1))
            || self
                .agent_navigation
                .announced_detachments
                .contains(&thread_id)
        {
            return;
        }
        let Ok(Ok(aliases)) = tokio::time::timeout(
            Duration::from_secs(/*secs*/ 5),
            app_server.agent_aliases(thread_id),
        )
        .await
        else {
            return;
        };
        if !aliases.iter().any(|alias| {
            alias.agent_ref == "1"
                && alias.thread_id == thread_id.to_string()
                && alias.state != AgentAliasState::Transferred
        }) {
            return;
        }
        let label = self.thread_label(thread_id);
        self.agent_navigation.mark_closed(thread_id);
        self.agent_navigation
            .set_parent_thread_id(thread_id, /*parent_thread_id*/ None);
        if let Ok(aliases) = app_server.agent_aliases(root).await {
            self.apply_primary_agent_aliases(aliases);
        }
        self.agent_navigation
            .announced_detachments
            .insert(thread_id);
        if !self
            .agent_navigation
            .pending_detachment_calls
            .values()
            .any(|targets| targets.contains(&thread_id))
        {
            self.chat_widget.add_info_message(
                format!(
                    "Detached {label}. Resume {thread_id} in another TUI to continue independently."
                ),
                /*hint*/ None,
            );
        }
        self.sync_active_agent_label();
    }

    async fn await_detached_agent_closures(
        &mut self,
        app_server: &mut AppServerSession,
        detached: &ThreadDetachResponse,
    ) -> Result<()> {
        let mut remaining = detached
            .unloaded_thread_ids
            .iter()
            .map(|id| ThreadId::from_string(id))
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
            while !remaining.is_empty() {
                let event = app_server.next_event().await.ok_or_else(|| {
                    color_eyre::eyre::eyre!("app-server disconnected during detach")
                })?;
                if event.backend_id == app_server.backend_id()
                    && let AppServerEvent::ServerNotification(notification) = &event.event
                    && let ServerNotification::ThreadClosed(closed) = notification.as_ref()
                    && let Ok(id) = ThreadId::from_string(&closed.thread_id)
                    && remaining.remove(&id)
                {
                    continue;
                }
                self.handle_backend_event(app_server, event).await;
            }
            Ok::<(), color_eyre::eyre::Report>(())
        })
        .await??;
        for id in &detached.unloaded_thread_ids {
            let id = ThreadId::from_string(id)?;
            self.abort_thread_event_listener(id);
            self.thread_event_channels.remove(&id);
            self.agent_navigation.mark_closed(id);
        }
        Ok(())
    }

    /// Keep an independently opened view and its draft when another client moves its branch.
    pub(super) async fn pause_after_agent_ownership_change(
        &mut self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
    ) -> bool {
        let Some(previous_root) = self.agent_navigation.root_thread_id() else {
            return false;
        };
        let Ok(Ok(aliases)) = tokio::time::timeout(
            Duration::from_secs(/*secs*/ 5),
            app_server.agent_aliases(thread_id),
        )
        .await
        else {
            return false;
        };
        let new_root = aliases
            .iter()
            .find(|alias| alias.agent_ref == "1")
            .and_then(|alias| ThreadId::from_string(&alias.thread_id).ok());
        if new_root.is_none_or(|new_root| new_root == previous_root) {
            return false;
        }
        self.history_recovery_required.insert(thread_id);
        self.abort_thread_event_listener(thread_id);
        self.active_thread_rx = None;
        self.active_thread_id = None;
        self.chat_widget.pause_unavailable_thread();
        self.chat_widget.add_info_message(
            "This conversation moved. Resume to continue; your draft is preserved.".into(),
            /*hint*/ None,
        );
        true
    }

    pub(super) async fn detach_displayed_agent(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        source: ThreadId,
    ) -> Result<()> {
        if self.chat_widget.thread_id() != Some(source) {
            return Ok(());
        }
        if let Err(error) = self.ensure_agent_control_admission(source, /*target*/ None) {
            self.chat_widget.add_error_message(error.to_string());
            return Ok(());
        }
        if self.windows_sandbox_blocks_thread_switch()
            || self.reject_pending_permission_root_switch()
        {
            self.chat_widget.add_error_message(
                "Finish the current permission or sandbox setup before detaching.".into(),
            );
            return Ok(());
        }
        let mut input = self.chat_widget.capture_thread_input_state();
        if let Some(input) = input.as_mut() {
            // Ownership changes never confirm an unsent queue for automatic submission.
            input.recovered_queue = true;
            input.reconnect_pending = true;
            input.questions = None;
            input.pending_thread_settings = None;
        }
        let detached = match app_server.thread_detach(source).await {
            Ok(detached) => detached,
            Err(error) => {
                // An error or lost reply can follow durable unload. Keep that old runtime's
                // queued closure from exiting the TUI and preserve input for explicit recovery.
                let still_loaded = matches!(
                    tokio::time::timeout(
                        Duration::from_secs(/*secs*/ 5),
                        app_server.thread_read(source, /*include_turns*/ false),
                    ).await,
                    Ok(Ok(thread)) if !matches!(thread.status, ThreadStatus::NotLoaded)
                );
                if still_loaded {
                    self.chat_widget.add_error_message(format!("{error:#}"));
                } else {
                    self.history_recovery_required.insert(source);
                    self.abort_thread_event_listener(source);
                    self.active_thread_rx = None;
                    self.active_thread_id = None;
                    self.chat_widget.pause_unavailable_thread();
                    self.chat_widget.add_error_message(format!(
                        "Detach could not finish: {error:#}. Your draft is preserved; resume this conversation to reload its current ownership."
                    ));
                }
                return Ok(());
            }
        };
        self.history_recovery_required.insert(source);
        self.abort_thread_event_listener(source);
        self.active_thread_rx = None;
        self.active_thread_id = None;
        let refreshed = async {
            self.await_detached_agent_closures(app_server, &detached)
                .await?;
            let backend_id = app_server.backend_id();
            self.resume_target_session(
                tui,
                app_server,
                SessionTarget {
                    backend_id,
                    path: None,
                    source_rollout_path: None,
                    thread_id: source,
                    cwd: Some(self.config.cwd.to_path_buf()),
                    history_mode: None,
                },
            )
            .await?;
            if self.active_thread_id != Some(source)
                || self.chat_widget.is_external_writer_view()
                || self.history_recovery_required.contains(&source)
            {
                color_eyre::eyre::bail!(
                    "detached, but the independent conversation could not reload"
                );
            }
            self.chat_widget.add_info_message(
                "Detached. This conversation and its agents now run independently.".into(),
                /*hint*/ None,
            );
            Ok::<(), color_eyre::eyre::Report>(())
        }
        .await;
        if refreshed.is_err() && self.chat_widget.thread_id() == Some(source) {
            self.chat_widget.restore_reconnected_input(input, &[]);
        }
        if let Err(error) = refreshed {
            self.chat_widget.pause_unavailable_thread();
            self.chat_widget.add_error_message(format!(
                "Detached, but refreshing failed: {error:#}. Resume this conversation to continue."
            ));
        }
        Ok(())
    }
}
