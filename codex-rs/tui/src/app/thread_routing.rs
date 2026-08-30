//! Thread routing, buffering, and app-server operation submission for the TUI app.
//!
//! This module manages active thread channels, routes server requests and notifications into those
//! channels, submits thread-scoped operations through the app server, and replays buffered events
//! when the visible thread changes.

use super::agent_observation_display::AgentResponseObservationBinding;
use super::agent_picker::AGENT_PICKER_VIEW_ID;
use super::session_lifecycle::ThreadAttachPresentation;
use super::*;
use crate::chatwidget::ThreadInputStateRestoreMode;
use crate::session_resume::read_saved_model_settings;
use codex_app_server_protocol::ThreadRollbackResponse;
use codex_app_server_protocol::ThreadStartedNotification;
use codex_app_server_protocol::TurnInterruptParams;
use codex_app_server_protocol::TurnInterruptResponse;
use codex_app_server_protocol::WarningNotification;

impl App {
    pub(super) async fn shutdown_current_thread(&mut self, app_server: &mut AppServerSession) {
        self.backtrack.pending_rollback = None;
        let side_thread_ids: Vec<ThreadId> = self.side_threads.keys().copied().collect();
        for side_thread_id in side_thread_ids {
            self.discard_side_thread(app_server, side_thread_id).await;
        }
        if let Some(thread_id) = self.chat_widget.thread_id() {
            if let Err(err) = app_server.thread_unsubscribe(thread_id).await {
                tracing::warn!("failed to unsubscribe thread {thread_id}: {err}");
            }
            self.abort_thread_event_listener(thread_id);
        }
    }

    pub(super) fn abort_thread_event_listener(&mut self, thread_id: ThreadId) {
        if let Some(handle) = self.thread_event_listener_tasks.remove(&thread_id) {
            handle.abort();
        }
    }

    pub(super) fn abort_all_thread_event_listeners(&mut self) {
        for handle in self
            .thread_event_listener_tasks
            .drain()
            .map(|(_, handle)| handle)
        {
            handle.abort();
        }
    }

    pub(super) fn ensure_thread_channel(&mut self, thread_id: ThreadId) -> &mut ThreadEventChannel {
        self.thread_event_channels
            .entry(thread_id)
            .or_insert_with(|| ThreadEventChannel::new(THREAD_EVENT_CHANNEL_CAPACITY))
    }

    pub(super) async fn set_thread_active(&mut self, thread_id: ThreadId, active: bool) {
        if let Some(channel) = self.thread_event_channels.get_mut(&thread_id) {
            let mut store = channel.store.lock().await;
            store.active = active;
        }
    }

    pub(super) async fn activate_thread_channel(&mut self, thread_id: ThreadId) {
        if self.active_thread_id.is_some() {
            return;
        }
        self.set_thread_active(thread_id, /*active*/ true).await;
        let receiver = if let Some(channel) = self.thread_event_channels.get_mut(&thread_id) {
            channel.receiver.take()
        } else {
            None
        };
        self.active_thread_id = Some(thread_id);
        self.active_thread_rx = receiver;
        self.refresh_pending_thread_approvals().await;
    }

    pub(super) async fn store_active_thread_receiver(&mut self) {
        let Some(active_id) = self.active_thread_id else {
            return;
        };
        let input_state = self.chat_widget.capture_thread_input_state();
        let recap_progress = self.recap.progress();
        if let Some(channel) = self.thread_event_channels.get_mut(&active_id) {
            let receiver = self.active_thread_rx.take();
            let mut store = channel.store.lock().await;
            store.active = false;
            store.set_input_state(input_state);
            store.merge_recap_progress(recap_progress);
            if let Some(receiver) = receiver {
                channel.receiver = Some(receiver);
            }
        }
    }

    pub(super) async fn activate_thread_for_replay(
        &mut self,
        thread_id: ThreadId,
    ) -> Option<(mpsc::Receiver<ThreadBufferedEvent>, ThreadEventSnapshot)> {
        let channel = self.thread_event_channels.get_mut(&thread_id)?;
        let receiver = channel.receiver.take()?;
        let mut store = channel.store.lock().await;
        store.active = true;
        let snapshot = store.snapshot();
        Some((receiver, snapshot))
    }

    pub(super) async fn clear_active_thread(&mut self) {
        if let Some(active_id) = self.active_thread_id.take() {
            self.set_thread_active(active_id, /*active*/ false).await;
        }
        self.active_thread_rx = None;
        self.refresh_pending_thread_approvals().await;
    }

    pub(super) async fn note_thread_outbound_op(&mut self, thread_id: ThreadId, op: &AppCommand) {
        let Some(channel) = self.thread_event_channels.get(&thread_id) else {
            return;
        };
        let mut store = channel.store.lock().await;
        store.note_outbound_op(op);
    }

    pub(super) async fn note_active_thread_outbound_op(&mut self, op: &AppCommand) {
        if !ThreadEventStore::op_can_change_pending_replay_state(op) {
            return;
        }
        let Some(thread_id) = self.active_thread_id else {
            return;
        };
        self.note_thread_outbound_op(thread_id, op).await;
    }

    pub(super) async fn active_turn_id_for_thread(&self, thread_id: ThreadId) -> Option<String> {
        let channel = self.thread_event_channels.get(&thread_id)?;
        let store = channel.store.lock().await;
        store.active_turn_id().map(ToOwned::to_owned)
    }

    pub(super) fn thread_label(&self, thread_id: ThreadId) -> String {
        let is_primary = self.agent_root_thread_id() == Some(thread_id);
        let fallback_label = if is_primary {
            "Main [default]".to_string()
        } else {
            let thread_id = thread_id.to_string();
            let short_id: String = thread_id.chars().take(8).collect();
            format!("Agent ({short_id})")
        };
        if let Some(entry) = self.agent_navigation.get(&thread_id) {
            let label = format_agent_picker_item_name(
                entry.agent_nickname.as_deref(),
                entry.agent_role.as_deref(),
                is_primary,
            );
            if label == "Agent" {
                let thread_id = thread_id.to_string();
                let short_id: String = thread_id.chars().take(8).collect();
                format!("{label} ({short_id})")
            } else {
                label
            }
        } else {
            fallback_label
        }
    }

    /// Returns the thread whose transcript is currently on screen.
    ///
    /// `active_thread_id` is the source of truth during steady state, but the widget can briefly
    /// lag behind thread bookkeeping during transitions. The footer label and adjacent-thread
    /// navigation both follow what the user is actually looking at, not whichever thread most
    /// recently began switching.
    pub(super) fn current_displayed_thread_id(&self) -> Option<ThreadId> {
        self.active_thread_id.or(self.chat_widget.thread_id())
    }

    pub(super) fn agent_root_thread_id(&self) -> Option<ThreadId> {
        self.agent_navigation
            .root_thread_id()
            .or(self.primary_thread_id)
    }

    pub(super) fn ignore_same_thread_resume(
        &mut self,
        target_session: &crate::resume_picker::SessionTarget,
    ) -> bool {
        if self.active_thread_id != Some(target_session.thread_id) {
            return false;
        };

        self.chat_widget.add_info_message(
            format!("Already viewing {}.", target_session.display_label()),
            /*hint*/ None,
        );
        true
    }

    /// Mirrors the visible thread into the contextual footer row.
    ///
    /// The footer sometimes shows ambient context instead of an instructional hint. In multi-agent
    /// sessions, that contextual row includes the currently viewed agent label. The label is
    /// intentionally hidden until there is more than one known thread so single-thread sessions do
    /// not spend footer space restating that the user is already on the main conversation.
    pub(super) fn sync_active_agent_label(&mut self) {
        let current_thread_id = self.current_displayed_thread_id();
        let agent_root_thread_id = self.agent_root_thread_id();
        let label = self
            .agent_navigation
            .active_agent_label(current_thread_id, agent_root_thread_id);
        self.chat_widget.set_active_agent_label(label);
        let mut targets = self
            .agent_navigation
            .ordered_threads()
            .into_iter()
            .filter(|(thread_id, _entry)| {
                Some(*thread_id) != current_thread_id
                    && self.agent_navigation.alias(*thread_id).is_none_or(|alias| {
                        alias.state != codex_app_server_protocol::AgentAliasState::Transferred
                    })
            })
            .map(|(thread_id, entry)| {
                let is_primary = agent_root_thread_id == Some(thread_id);
                let label = format_agent_picker_item_name(
                    entry.agent_nickname.as_deref(),
                    entry.agent_role.as_deref(),
                    is_primary,
                );
                AgentPromptTarget {
                    thread_id: Some(thread_id),
                    selector: if is_primary {
                        codex_protocol::MAIN_AGENT_NICKNAME.to_string()
                    } else {
                        self.agent_navigation
                            .control_selector(thread_id)
                            .unwrap_or_else(|| thread_id.to_string())
                    },
                    label: if entry.is_closed {
                        format!("{label} · closed")
                    } else {
                        label
                    },
                }
            })
            .collect::<Vec<_>>();
        targets.push(AgentPromptTarget {
            thread_id: None,
            selector: "new".to_string(),
            label: "New default agent".to_string(),
        });
        targets.extend(
            AGENT_TARGET_ACTION_CHOICES.map(|(selector, label)| AgentPromptTarget {
                thread_id: None,
                selector: selector.to_string(),
                label: label.to_string(),
            }),
        );
        targets.extend(
            self.config
                .agent_roles
                .keys()
                .map(|role| AgentPromptTarget {
                    thread_id: None,
                    selector: agent_role_selector(role),
                    label: format!("New {role} agent"),
                }),
        );
        self.chat_widget.set_agent_prompt_targets(targets);
        self.sync_side_thread_ui();
    }

    pub(super) async fn thread_cwd(&self, thread_id: ThreadId) -> Option<AbsolutePathBuf> {
        let channel = self.thread_event_channels.get(&thread_id)?;
        let store = channel.store.lock().await;
        store.session.as_ref().map(|session| session.cwd.clone())
    }

    pub(super) async fn interactive_request_for_thread_request(
        &self,
        thread_id: ThreadId,
        request: &ServerRequest,
    ) -> std::io::Result<Option<ThreadInteractiveRequest>> {
        let thread_label = Some(self.thread_label(thread_id));
        Ok(match request {
            ServerRequest::CommandExecutionRequestApproval { params, .. } => {
                let network_approval_context = params.network_approval_context.clone();
                let additional_permissions = params.additional_permissions.clone();
                let proposed_execpolicy_amendment = params.proposed_execpolicy_amendment.clone();
                let proposed_network_policy_amendments =
                    params.proposed_network_policy_amendments.clone();
                let approval = ExecApprovalRequest {
                    kind: params.kind,
                    thread_id,
                    thread_label,
                    id: params
                        .approval_id
                        .clone()
                        .unwrap_or_else(|| params.item_id.clone()),
                    environment_id: params.environment_id.clone(),
                    started_at_ms: params.started_at_ms.unwrap_or_default(),
                    expires_at_ms: params.started_at_ms.and(params.expires_at_ms),
                    received_at: self
                        .pending_app_server_requests
                        .request_received_at(request)
                        .unwrap_or_else(std::time::Instant::now),
                    command: params
                        .command
                        .as_deref()
                        .map(split_command_string)
                        .unwrap_or_default(),
                    reason: params.reason.clone(),
                    available_decisions: params.available_decisions.clone().unwrap_or_else(|| {
                        default_exec_approval_decisions(
                            network_approval_context.as_ref(),
                            proposed_execpolicy_amendment.as_ref(),
                            proposed_network_policy_amendments.as_deref(),
                            additional_permissions.as_ref(),
                        )
                    }),
                    network_approval_context,
                    additional_permissions,
                };
                Some(ThreadInteractiveRequest::Approval(ApprovalRequest::Exec(
                    approval,
                )))
            }
            ServerRequest::FileChangeRequestApproval { params, .. } => Some(
                ThreadInteractiveRequest::Approval(ApprovalRequest::ApplyPatch(
                    ApplyPatchApprovalRequest {
                    thread_id,
                    thread_label,
                    id: params.item_id.clone(),
                    reason: params.reason.clone(),
                    cwd: self
                        .thread_cwd(thread_id)
                        .await
                        .unwrap_or_else(|| self.config.cwd.clone()),
                    changes: self
                        .thread_file_change_changes(thread_id, &params.turn_id, &params.item_id)
                        .await
                        .map(crate::app_server_approval_conversions::file_update_changes_to_display)
                        .unwrap_or_default(),
                    },
                )),
            ),
            ServerRequest::McpServerElicitationRequest { request_id, params } => {
                if let Some(params) = AppLinkViewParams::from_url_app_server_request(
                    thread_id,
                    &params.server_name,
                    request_id.clone(),
                    &params.request,
                ) {
                    Some(ThreadInteractiveRequest::AppLink(params))
                } else if let Some(request) =
                    McpServerElicitationFormRequest::from_app_server_request(
                        thread_id,
                        request_id.clone(),
                        params,
                    )
                {
                    Some(ThreadInteractiveRequest::McpServerElicitation(request))
                } else {
                    match &params.request {
                        codex_app_server_protocol::McpServerElicitationRequest::Form {
                            message,
                            ..
                        } => Some(ThreadInteractiveRequest::Approval(
                            ApprovalRequest::McpElicitation(McpElicitationApprovalRequest {
                                thread_id,
                                thread_label,
                                server_name: params.server_name.clone(),
                                request_id: request_id.clone(),
                                message: message.clone(),
                            }),
                        )),
                        codex_app_server_protocol::McpServerElicitationRequest::OpenAiForm {
                            ..
                        }
                        | codex_app_server_protocol::McpServerElicitationRequest::OpenAiElicitationForm { .. }
                        | codex_app_server_protocol::McpServerElicitationRequest::Url { .. } => {
                            self.app_event_tx.resolve_elicitation(
                                thread_id,
                                params.server_name.clone(),
                                request_id.clone(),
                                codex_app_server_protocol::McpServerElicitationAction::Decline,
                                /*content*/ None,
                                /*meta*/ None,
                            );
                            None
                        }
                    }
                }
            }
            ServerRequest::PermissionsRequestApproval { params, .. } => {
                // TODO(anp): Remove this native-path localization error path once core permission
                // paths remain PathUri after crossing the app-server boundary.
                let permissions = params.permissions.clone().try_into().map_err(|err| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("failed to localize requested filesystem paths: {err}"),
                    )
                })?;
                Some(ThreadInteractiveRequest::Approval(
                    ApprovalRequest::Permissions(PermissionsApprovalRequest {
                        thread_id,
                        thread_label,
                        call_id: params.item_id.clone(),
                        environment_id: params.environment_id.clone(),
                        reason: params.reason.clone(),
                        permissions,
                    }),
                ))
            }
            _ => None,
        })
    }

    pub(super) fn push_thread_interactive_request(&mut self, request: ThreadInteractiveRequest) {
        if self.chat_widget.has_misalignment_policy_violation() {
            return;
        }

        match request {
            ThreadInteractiveRequest::AppLink(params) => {
                self.chat_widget.open_app_link_view(params);
            }
            ThreadInteractiveRequest::Approval(request) => {
                self.render_inactive_patch_preview(&request);
                self.chat_widget.push_approval_request(request);
                if self.startup_protected_input_boundary && !self.chat_widget.has_active_view() {
                    self.startup_pending_protected_request = true;
                }
            }
            ThreadInteractiveRequest::McpServerElicitation(request) => {
                self.chat_widget
                    .push_mcp_server_elicitation_request(request);
            }
        }
    }

    fn render_inactive_patch_preview(&mut self, request: &ApprovalRequest) {
        let ApprovalRequest::ApplyPatch(request) = request else {
            return;
        };
        if request.thread_label.is_none() || request.changes.is_empty() {
            return;
        }
        self.chat_widget
            .add_to_history(history_cell::new_patch_event(
                request.changes.clone(),
                &request.cwd,
            ));
    }

    pub(super) async fn pending_inactive_thread_requests(&self) -> Vec<(ThreadId, ServerRequest)> {
        let channels: Vec<(ThreadId, Arc<Mutex<ThreadEventStore>>)> = self
            .thread_event_channels
            .iter()
            .map(|(thread_id, channel)| (*thread_id, Arc::clone(&channel.store)))
            .collect();

        let mut requests = Vec::new();
        for (thread_id, store) in channels {
            if Some(thread_id) == self.active_thread_id {
                continue;
            }

            let store = store.lock().await;
            if store.side_parent_pending_status().is_none() {
                continue;
            }
            requests.extend(
                store
                    .pending_replay_requests()
                    .into_iter()
                    .map(|request| (thread_id, request)),
            );
        }
        requests
    }

    pub(super) async fn surface_pending_inactive_thread_interactive_requests(
        &mut self,
    ) -> Result<()> {
        if self.active_side_parent_thread_id().is_some() {
            return Ok(());
        }

        let requests = self.pending_inactive_thread_requests().await;
        for (thread_id, request) in requests {
            if let Some(request) = self
                .interactive_request_for_thread_request(thread_id, &request)
                .await?
            {
                self.push_thread_interactive_request(request);
            }
        }
        Ok(())
    }

    pub(super) async fn submit_active_thread_op(
        &mut self,
        app_server: &mut AppServerSession,
        op: AppCommand,
    ) -> Result<()> {
        let Some(thread_id) = self.active_thread_id else {
            self.chat_widget
                .add_error_message("No active thread is available.".to_string());
            return Ok(());
        };

        self.submit_thread_op(app_server, thread_id, op).await
    }

    pub(super) async fn submit_thread_op(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        op: AppCommand,
    ) -> Result<()> {
        if self.thread_unavailable(thread_id) {
            self.chat_widget.add_error_message(
                "This conversation is unavailable; no operation was sent.".into(),
            );
            return Ok(());
        }
        if self.chat_widget.rejects_misalignment_policy_op(&op) {
            return Ok(());
        }

        crate::session_log::log_outbound_op(&op);

        if self
            .try_resolve_app_server_request(app_server, thread_id, &op)
            .await?
        {
            return Ok(());
        }

        let requires_live_thread = matches!(
            &op,
            AppCommand::Interrupt
                | AppCommand::CleanBackgroundTerminals
                | AppCommand::TerminateBackgroundTerminal { .. }
                | AppCommand::RealtimeConversationStart { .. }
                | AppCommand::RealtimeConversationAudio(_)
                | AppCommand::RealtimeConversationClose
                | AppCommand::RunUserShellCommand { .. }
                | AppCommand::UserTurn { .. }
                | AppCommand::OverrideTurnContext { .. }
                | AppCommand::Compact
                | AppCommand::Review { .. }
                | AppCommand::ActivateMcpServer { .. }
                | AppCommand::ApproveGuardianDeniedAction { .. }
        );
        if requires_live_thread
            && let Err(error) = self.resume_replay_only_thread(app_server, thread_id).await
        {
            tracing::warn!(
                thread_id = %thread_id,
                error = %error,
                "failed to resume replay-only thread for live operation"
            );
            let message = format!("Failed to resume agent thread: {error:#}");
            if matches!(&op, AppCommand::UserTurn { .. })
                && self
                    .chat_widget
                    .handle_turn_start_rejection(message.clone())
            {
                return Ok(());
            }
            self.chat_widget.add_error_message(message);
            return Ok(());
        }

        if self
            .try_submit_active_thread_op_via_app_server(app_server, thread_id, &op)
            .await?
        {
            if ThreadEventStore::op_can_change_pending_replay_state(&op) {
                self.note_thread_outbound_op(thread_id, &op).await;
                self.refresh_pending_thread_approvals().await;
                self.refresh_side_parent_status_from_store(thread_id).await;
            }
            return Ok(());
        }

        self.chat_widget
            .add_error_message(format!("Not available in TUI yet for thread {thread_id}."));
        Ok(())
    }

    /// Persist prompt text in the local cross-session message history.
    pub(super) fn append_message_history_entry(&self, thread_id: ThreadId, text: String) {
        let history_config = codex_message_history::HistoryConfig::new(
            self.chat_widget.config_ref().codex_home.clone(),
            &self.chat_widget.config_ref().history,
        );
        tokio::spawn(async move {
            if let Err(err) =
                codex_message_history::append_entry(&text, thread_id, &history_config).await
            {
                tracing::warn!(
                    thread_id = %thread_id,
                    error = %err,
                    "failed to append to message history"
                );
            }
        });
    }

    /// Fetch one local cross-session message history entry for the requesting thread.
    pub(super) async fn lookup_message_history_entry(
        &mut self,
        thread_id: ThreadId,
        offset: usize,
        log_id: u64,
    ) -> Result<()> {
        let history_config = codex_message_history::HistoryConfig::new(
            self.chat_widget.config_ref().codex_home.clone(),
            &self.chat_widget.config_ref().history,
        );
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let entry_opt = tokio::task::spawn_blocking(move || {
                codex_message_history::lookup(log_id, offset, &history_config)
            })
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(error = %err, "history lookup task failed");
                None
            });

            app_event_tx.send(AppEvent::ThreadHistoryEntryResponse {
                thread_id,
                event: HistoryLookupResponse::Entry {
                    offset,
                    log_id,
                    entry: entry_opt.map(|entry| entry.text),
                },
            });
        });
        Ok(())
    }

    /// Fetch one bounded local cross-session history batch for the requesting thread.
    pub(super) async fn lookup_message_history_batch(
        &mut self,
        thread_id: ThreadId,
        cursor: codex_message_history::HistoryBatchCursor,
        log_id: u64,
    ) -> Result<()> {
        let history_config = codex_message_history::HistoryConfig::new(
            self.chat_widget.config_ref().codex_home.clone(),
            &self.chat_widget.config_ref().history,
        );
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let event = match tokio::task::spawn_blocking(move || {
                codex_message_history::lookup_batch(log_id, cursor, &history_config)
            })
            .await
            {
                Ok(Ok(batch)) => {
                    let entries = batch
                        .entries
                        .into_iter()
                        .map(|entry| crate::app_event::HistoryBatchEntryResponse {
                            offset: entry.offset,
                            entry: entry.entry.map(|entry| entry.text),
                        })
                        .collect();
                    HistoryLookupResponse::Batch {
                        cursor,
                        log_id,
                        entries,
                        next_older_cursor: batch.next_older_cursor,
                    }
                }
                Ok(Err(err)) => {
                    tracing::warn!(error = %err, "history batch lookup failed");
                    HistoryLookupResponse::BatchError { cursor, log_id }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "history batch lookup task failed");
                    HistoryLookupResponse::BatchError { cursor, log_id }
                }
            };

            app_event_tx.send(AppEvent::ThreadHistoryEntryResponse { thread_id, event });
        });
        Ok(())
    }

    pub(super) async fn try_submit_active_thread_op_via_app_server(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        op: &AppCommand,
    ) -> Result<bool> {
        match op {
            AppCommand::Interrupt => {
                let mut turn_id = self
                    .active_turn_id_for_thread(thread_id)
                    .await
                    .unwrap_or_default();
                let (thread_event_tx, thread_event_store) = {
                    let channel = self.ensure_thread_channel(thread_id);
                    (channel.sender.clone(), Arc::clone(&channel.store))
                };
                self.reset_backtrack_state();
                if !turn_id.is_empty() {
                    let mut store = thread_event_store.lock().await;
                    if store.pending_interrupt_turn_id.as_deref() == Some(turn_id.as_str()) {
                        return Ok(true);
                    }
                    store.pending_interrupt_turn_id = Some(turn_id.clone());
                }
                let request_handle = app_server.request_handle();
                let request_ids = [app_server.next_request_id(), app_server.next_request_id()];
                tokio::spawn(async move {
                    for (attempt, request_id) in request_ids.into_iter().enumerate() {
                        let result = request_handle
                            .request_typed::<TurnInterruptResponse>(ClientRequest::TurnInterrupt {
                                request_id,
                                params: TurnInterruptParams {
                                    thread_id: thread_id.to_string(),
                                    turn_id: turn_id.clone(),
                                },
                            })
                            .await;

                        match result {
                            Ok(_) => break,
                            Err(error) => {
                                if attempt == 0
                                    && let Some(actual_turn_id) = active_turn_interrupt_race(&error)
                                    && actual_turn_id != turn_id
                                {
                                    thread_event_store.lock().await.pending_interrupt_turn_id =
                                        Some(actual_turn_id.clone());
                                    turn_id = actual_turn_id;
                                    continue;
                                }
                                tracing::warn!(error = %error, "turn/interrupt failed in TUI");
                                let notification =
                                    ServerNotification::Warning(WarningNotification {
                                        thread_id: Some(thread_id.to_string()),
                                        message: format!("Failed to interrupt turn: {error}"),
                                    });
                                let should_send = {
                                    let mut store = thread_event_store.lock().await;
                                    store.push_notification_ref(&notification);
                                    store.active
                                };
                                if should_send
                                    && let Err(error) = thread_event_tx
                                        .send(ThreadBufferedEvent::Notification(Box::new(
                                            notification,
                                        )))
                                        .await
                                {
                                    tracing::warn!(error = %error, "thread event channel closed");
                                }
                                break;
                            }
                        }
                    }
                });
                Ok(true)
            }
            AppCommand::UserTurn {
                items,
                cwd,
                approval_policy,
                approvals_reviewer,
                active_permission_profile,
                model,
                effort,
                summary,
                service_tier,
                final_output_json_schema,
                collaboration_mode,
                personality,
            } => {
                let mut should_start_turn = true;
                if let Some(turn_id) = self.active_turn_id_for_thread(thread_id).await {
                    let mut steer_turn_id = turn_id;
                    let mut retried_after_turn_mismatch = false;
                    loop {
                        match app_server
                            .turn_steer(thread_id, steer_turn_id.clone(), items.to_vec())
                            .await
                        {
                            Ok(_) => return Ok(true),
                            Err(error) => {
                                if let Some(turn_error) =
                                    active_turn_not_steerable_turn_error(&error)
                                {
                                    if !self.chat_widget.enqueue_rejected_steer() {
                                        self.chat_widget.add_error_message(turn_error.message);
                                    }
                                    return Ok(true);
                                }
                                match active_turn_steer_race(&error) {
                                    Some(ActiveTurnSteerRace::Missing) => {
                                        if let Some(channel) =
                                            self.thread_event_channels.get(&thread_id)
                                        {
                                            let mut store = channel.store.lock().await;
                                            store.clear_active_turn_id();
                                        }
                                        should_start_turn = true;
                                        break;
                                    }
                                    Some(ActiveTurnSteerRace::ExpectedTurnMismatch {
                                        actual_turn_id,
                                    }) if !retried_after_turn_mismatch
                                        && actual_turn_id != steer_turn_id =>
                                    {
                                        // Review flows can swap the active turn before the TUI
                                        // processes the corresponding notification. Retry once with
                                        // the server-reported turn id so non-steerable review turns
                                        // still fall through to the existing queueing behavior.
                                        if let Some(channel) =
                                            self.thread_event_channels.get(&thread_id)
                                        {
                                            let mut store = channel.store.lock().await;
                                            store.set_active_turn_id(actual_turn_id.clone());
                                        }
                                        steer_turn_id = actual_turn_id;
                                        retried_after_turn_mismatch = true;
                                    }
                                    Some(ActiveTurnSteerRace::ExpectedTurnMismatch {
                                        actual_turn_id,
                                    }) => {
                                        if let Some(channel) =
                                            self.thread_event_channels.get(&thread_id)
                                        {
                                            let mut store = channel.store.lock().await;
                                            store.set_active_turn_id(actual_turn_id);
                                        }
                                        return Err(error.into());
                                    }
                                    None => return Err(error.into()),
                                }
                            }
                        }
                    }
                }
                if should_start_turn {
                    let config = self.chat_widget.config_ref();
                    let approvals_reviewer =
                        approvals_reviewer.unwrap_or(config.approvals_reviewer);
                    let permissions_override = Self::turn_permissions_override_from_config(
                        config,
                        active_permission_profile.as_ref(),
                        self.runtime_permission_profile_override
                            .as_ref()
                            .and_then(RuntimePermissionProfileOverride::turn_permission_profile),
                    );
                    let response = app_server
                        .turn_start(
                            thread_id,
                            items.to_vec(),
                            cwd.clone(),
                            *approval_policy,
                            approvals_reviewer,
                            permissions_override,
                            config.permissions.user_visible_workspace_roots(),
                            model.to_string(),
                            effort.clone(),
                            *summary,
                            service_tier.clone(),
                            collaboration_mode.clone(),
                            *personality,
                            final_output_json_schema.clone(),
                        )
                        .await?;
                    if self.active_thread_id == Some(thread_id)
                        && self.chat_widget.thread_id() == Some(thread_id)
                    {
                        self.chat_widget
                            .record_safety_buffering_turn(response.turn.id, op);
                    }
                }
                Ok(true)
            }
            AppCommand::ListSkills { cwds, force_reload } => {
                self.handle_skills_list_result(
                    app_server
                        .skills_list(codex_app_server_protocol::SkillsListParams {
                            cwds: cwds.clone(),
                            force_reload: *force_reload,
                        })
                        .await,
                    "failed to refresh skills",
                );
                Ok(true)
            }
            AppCommand::Compact => {
                app_server.thread_compact_start(thread_id).await?;
                Ok(true)
            }
            AppCommand::SetThreadName { name } => {
                let name = name.to_string();
                app_server.thread_set_name(thread_id, name.clone()).await?;
                self.chat_widget.expect_manual_thread_name(thread_id, name);
                Ok(true)
            }
            AppCommand::Review { target } => {
                let response = app_server.review_start(thread_id, target.clone()).await?;
                let review_thread_id = ThreadId::from_string(&response.review_thread_id)
                    .wrap_err("review/start returned invalid review thread id")?;
                let store = Arc::clone(&self.ensure_thread_channel(review_thread_id).store);
                let mut store = store.lock().await;
                store.set_active_turn_id(response.turn.id);
                Ok(true)
            }
            AppCommand::CleanBackgroundTerminals => {
                app_server
                    .thread_background_terminals_clean(thread_id)
                    .await?;
                Ok(true)
            }
            AppCommand::TerminateBackgroundTerminal { process_id } => {
                let terminated = app_server
                    .thread_background_terminal_terminate(thread_id, *process_id)
                    .await?;
                if terminated {
                    self.chat_widget.add_info_message(
                        format!("Stopping background terminal {process_id}."),
                        /*hint*/ None,
                    );
                } else {
                    self.chat_widget.add_error_message(format!(
                        "Background terminal {process_id} was not found."
                    ));
                }
                Ok(true)
            }
            AppCommand::RealtimeConversationStart { transport, voice } => {
                app_server
                    .thread_realtime_start(thread_id, transport.clone(), voice.clone())
                    .await?;
                Ok(true)
            }
            AppCommand::RealtimeConversationAudio(frame) => {
                app_server
                    .thread_realtime_audio(thread_id, frame.clone())
                    .await?;
                Ok(true)
            }
            AppCommand::RealtimeConversationClose => {
                app_server.thread_realtime_stop(thread_id).await?;
                Ok(true)
            }
            AppCommand::RunUserShellCommand {
                command,
                response_handling,
            } => {
                app_server
                    .thread_shell_command(thread_id, command.to_string(), Some(*response_handling))
                    .await?;
                Ok(true)
            }
            AppCommand::ReloadUserConfig => {
                app_server.reload_user_config().await?;
                Ok(true)
            }
            AppCommand::OverrideTurnContext { .. } => {
                self.sync_override_turn_context_settings(app_server, thread_id, op)
                    .await;
                Ok(true)
            }
            AppCommand::ActivateMcpServer { server_name } => {
                let outcome = match app_server
                    .thread_mcp_server_activate(thread_id, server_name.clone())
                    .await
                {
                    Ok(outcome) => outcome,
                    Err(err) => {
                        tracing::warn!(
                            thread_id = %thread_id,
                            server_name = %server_name,
                            "failed to queue MCP server prompt context: {err:#}"
                        );
                        self.chat_widget.add_error_message(format!(
                            "Failed to use MCP server `{server_name}`: {err:#}"
                        ));
                        return Ok(true);
                    }
                };
                match outcome {
                    codex_app_server_protocol::ThreadMcpServerActivateOutcome::Activated => {
                        self.chat_widget.add_info_message(
                            format!(
                                "MCP server `{server_name}` tools added to context."
                            ),
                            /*hint*/ None,
                        );
                    }
                    codex_app_server_protocol::ThreadMcpServerActivateOutcome::AlreadyActivated => {
                        self.chat_widget.add_info_message(
                            format!(
                                "MCP server `{server_name}` tools are already in context."
                            ),
                            /*hint*/ None,
                        );
                    }
                    codex_app_server_protocol::ThreadMcpServerActivateOutcome::AlreadyImplicitlyAvailable => {
                        self.chat_widget.add_info_message(
                            format!(
                                "MCP server `{server_name}` is already visible to the model by default."
                            ),
                            /*hint*/ None,
                        );
                    }
                }
                Ok(true)
            }
            AppCommand::ApproveGuardianDeniedAction { event } => {
                app_server
                    .thread_approve_guardian_denied_action(thread_id, event)
                    .await?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub(super) fn turn_permissions_override_from_config(
        config: &Config,
        active_permission_profile: Option<&ActivePermissionProfile>,
        runtime_permission_profile_override: Option<&PermissionProfile>,
    ) -> TurnPermissionsOverride {
        if let Some(active_permission_profile) = active_permission_profile {
            return TurnPermissionsOverride::ActiveProfile(active_permission_profile.clone());
        }

        let effective_permission_profile = config.permissions.effective_permission_profile();
        let runtime_permission_profile_override =
            runtime_permission_profile_override.map(|profile| {
                profile
                    .clone()
                    .materialize_project_roots_with_workspace_roots(
                        &config.effective_workspace_roots(),
                    )
            });
        if runtime_permission_profile_override
            .as_ref()
            .is_some_and(|profile| profile == &effective_permission_profile)
        {
            return TurnPermissionsOverride::LegacySandbox(effective_permission_profile);
        }

        TurnPermissionsOverride::Preserve
    }

    pub(super) fn handle_skills_list_result(
        &mut self,
        result: Result<SkillsListResponse>,
        failure_message: &str,
    ) {
        match result {
            Ok(response) => self.handle_skills_list_response(response),
            Err(err) => {
                tracing::warn!("{failure_message}: {err:#}");
                self.chat_widget
                    .add_error_message(format!("{failure_message}: {err:#}"));
            }
        }
    }

    pub(super) async fn try_resolve_app_server_request(
        &mut self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
        op: &AppCommand,
    ) -> Result<bool> {
        let thread_id_string = thread_id.to_string();
        let Some(resolution) = self
            .pending_app_server_requests
            .take_resolution_for_thread(&thread_id_string, op)
            .map_err(|err| color_eyre::eyre::eyre!(err))?
        else {
            return Ok(false);
        };

        match app_server
            .resolve_server_request(resolution.request_id, resolution.result)
            .await
        {
            Ok(()) => {
                if ThreadEventStore::op_can_change_pending_replay_state(op) {
                    self.note_thread_outbound_op(thread_id, op).await;
                    self.refresh_pending_thread_approvals().await;
                    self.refresh_side_parent_status_from_store(thread_id).await;
                }
                Ok(true)
            }
            Err(err) => {
                self.chat_widget.add_error_message(format!(
                    "Failed to resolve app-server request for thread {thread_id}: {err}"
                ));
                Ok(false)
            }
        }
    }

    pub(super) async fn refresh_pending_thread_approvals(&mut self) {
        let side_parent_thread_id = self.active_side_parent_thread_id();
        let channels: Vec<(ThreadId, Arc<Mutex<ThreadEventStore>>)> = self
            .thread_event_channels
            .iter()
            .map(|(thread_id, channel)| (*thread_id, Arc::clone(&channel.store)))
            .collect();

        let mut pending_thread_ids = Vec::new();
        for (thread_id, store) in channels {
            if Some(thread_id) == self.active_thread_id || Some(thread_id) == side_parent_thread_id
            {
                continue;
            }

            let store = store.lock().await;
            if store.has_pending_thread_approvals() {
                pending_thread_ids.push(thread_id);
            }
        }

        pending_thread_ids.sort_by_key(ThreadId::to_string);

        let threads = pending_thread_ids
            .into_iter()
            .map(|thread_id| self.thread_label(thread_id))
            .collect();

        self.chat_widget.set_pending_thread_approvals(threads);
        if let Some(selected) = self
            .chat_widget
            .selected_index_for_present_view(AGENT_PICKER_VIEW_ID)
        {
            let params = self.agent_picker_selection_view_params(Some(selected));
            self.chat_widget
                .replace_selection_view_if_present(AGENT_PICKER_VIEW_ID, params);
        }
    }

    pub(super) async fn refresh_side_parent_status_from_store(&mut self, thread_id: ThreadId) {
        let Some(channel) = self.thread_event_channels.get(&thread_id) else {
            return;
        };
        let status = {
            let store = channel.store.lock().await;
            store.side_parent_pending_status()
        };
        if let Some(status) = status {
            self.set_side_parent_status(thread_id, Some(status));
        } else {
            self.clear_side_parent_action_status(thread_id);
        }
    }

    pub(super) async fn enqueue_thread_notification(
        &mut self,
        thread_id: ThreadId,
        notification: ServerNotification,
    ) -> Result<()> {
        if self.abandoned_side_threads.contains(&thread_id) {
            return Ok(());
        }
        if self.current_displayed_thread_id() == Some(thread_id)
            && let ServerNotification::TurnCompleted(notification) = &notification
        {
            let now = Instant::now();

            self.recap
                .note_turn_finished(&notification.turn.status, now);
            self.schedule_recap_check(thread_id, now);
        }
        let misalignment_policy_violation =
            match &notification {
                ServerNotification::Error(notification) if !notification.will_retry => {
                    notification.error.codex_error_info.as_ref()
                }
                ServerNotification::TurnCompleted(notification) => notification
                    .turn
                    .error
                    .as_ref()
                    .and_then(|error| error.codex_error_info.as_ref()),
                _ => None,
            } == Some(&AppServerCodexErrorInfo::MisalignmentPolicyViolation);
        if misalignment_policy_violation && self.active_side_parent_thread_id() == Some(thread_id) {
            if self.chat_widget.is_user_turn_pending_or_running() {
                self.chat_widget.submit_op(AppCommand::Interrupt);
            }
            self.chat_widget.on_misalignment_policy_violation();
        }
        if matches!(
            notification,
            ServerNotification::ThreadSettingsUpdated(_) | ServerNotification::ThreadArchived(_)
        ) && self.primary_thread_id.is_some()
            && self.primary_thread_id != Some(thread_id)
            && !self.thread_event_channels.contains_key(&thread_id)
        {
            return Ok(());
        }
        if let ServerNotification::ThreadSettingsUpdated(notification) = &notification {
            self.apply_thread_settings_to_cached_session(thread_id, &notification.thread_settings)
                .await;
        }
        self.cache_collab_response_observation_for_notification(&notification);
        let inferred_session = if let ServerNotification::ThreadStarted(started) = &notification
            && self.primary_session_configured.is_some()
        {
            let agent_nickname = self
                .agent_navigation
                .authoritative_nickname(thread_id, started.thread.agent_nickname.clone());
            self.upsert_agent_picker_thread(
                thread_id,
                agent_nickname,
                started.thread.agent_role.clone(),
                /*is_closed*/ false,
            );
            self.agent_navigation.set_parent_thread_id(
                thread_id,
                crate::app_server_session::thread_parent_thread_id(&started.thread),
            );

            // Lifecycle responses already contain authoritative session state. Their rollout may
            // not exist until the first turn, so inferring it again can wait through reader retries.
            let already_has_session = match self.thread_event_channels.get(&thread_id) {
                Some(channel) => channel.store.lock().await.session.is_some(),
                None => false,
            };

            if already_has_session {
                None
            } else {
                self.infer_session_for_started_thread(thread_id, started)
                    .await
            }
        } else {
            None
        };
        let turn_started_agent_queue = match &notification {
            ServerNotification::TurnStarted(notification) => notification.agent_queue.clone(),
            _ => None,
        };
        let is_turn_started = matches!(notification, ServerNotification::TurnStarted(_));
        let is_thread_closed = matches!(notification, ServerNotification::ThreadClosed(_));
        let notification_status_change = SideParentStatusChange::for_notification(&notification);
        let (sender, store) = {
            let channel = self.ensure_thread_channel(thread_id);
            (channel.sender.clone(), Arc::clone(&channel.store))
        };
        let (notification, previous_pending_status, pending_status, turn_stopped) = {
            let mut guard = store.lock().await;
            if guard.session.is_none()
                && let Some(session) = inferred_session
            {
                guard.session = Some(session);
            }
            let previous_pending_status = guard.side_parent_pending_status();
            let turn_stopped = match &notification {
                ServerNotification::TurnCompleted(notification) => {
                    guard.active_turn_id() == Some(notification.turn.id.as_str())
                }
                _ => false,
            };
            let notification = if guard.active {
                guard.push_notification_ref(&notification);
                Some(notification)
            } else {
                guard.push_notification(notification);
                None
            };
            (
                notification,
                previous_pending_status,
                guard.side_parent_pending_status(),
                turn_stopped,
            )
        };
        if is_turn_started {
            self.agent_navigation.mark_running(thread_id);
            if let Some(agent_queue) = turn_started_agent_queue {
                match ThreadId::from_string(&agent_queue.source_thread_id) {
                    Ok(source_thread_id) => {
                        if let Some(response_handling) = agent_queue.response_handling {
                            self.agent_navigation.note_response_observation(
                                source_thread_id,
                                thread_id,
                                AgentResponseObservationBinding::Bound,
                                Some(response_handling),
                            );
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            source_thread_id = agent_queue.source_thread_id,
                            %err,
                            "ignoring queued turn response handling with invalid source thread id"
                        );
                    }
                }
            }
            if self.queued_agent_prompts.contains_key(&thread_id) {
                self.app_event_tx.send(AppEvent::RefreshAgentPromptQueue);
            }
        } else if is_thread_closed {
            // A closed inactive thread does not pass through `handle_active_thread_event`, so
            // perform the same lifecycle cleanup at notification ingress. In particular, a
            // queued follow-up must not turn a close into an implicit resume.
            self.mark_agent_picker_thread_closed(thread_id);
        } else if turn_stopped {
            self.agent_navigation.mark_stopped(thread_id);
            if self.queued_agent_prompts.contains_key(&thread_id) {
                self.app_event_tx.send(AppEvent::RefreshAgentPromptQueue);
            }
        }

        if let Some(notification) = notification {
            match sender.try_send(ThreadBufferedEvent::Notification(Box::new(notification))) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    tokio::spawn(async move {
                        if let Err(err) = sender.send(event).await {
                            tracing::warn!("thread {thread_id} event channel closed: {err}");
                        }
                    });
                }
                Err(TrySendError::Closed(_)) => {
                    tracing::warn!("thread {thread_id} event channel closed");
                }
            }
        }
        if let Some(status) = pending_status {
            self.set_side_parent_status(thread_id, Some(status));
        } else if let Some(change) = notification_status_change {
            self.apply_side_parent_status_change(thread_id, change);
        } else if previous_pending_status.is_some() {
            self.clear_side_parent_action_status(thread_id);
        }
        self.refresh_pending_thread_approvals().await;
        Ok(())
    }

    /// Locally remembers receiver threads referenced by a collab notification.
    ///
    /// This intentionally avoids app-server reads on the active-thread rendering path. During large
    /// fan-outs the app-server can be saturated with spawn work, and blocking here would freeze the
    /// TUI event loop. Metadata from `ThreadStarted` or explicit picker refreshes still fills in
    /// names and roles later; until then, rendering falls back to the thread id.
    pub(super) fn cache_collab_receiver_threads_for_notification(
        &mut self,
        notification: &ServerNotification,
    ) {
        if let Some(activity) =
            sub_agent_activity_item(notification).and_then(sub_agent_activity_display)
        {
            self.agent_navigation.record_sub_agent_activity(activity);
            self.sync_active_agent_label();
            return;
        }

        if let Some(receiver_agents) = collab_receiver_agents(notification) {
            for receiver in receiver_agents {
                if collab_receiver_is_not_found(notification, &receiver.thread_id) {
                    continue;
                }
                let Some(thread_id) = crate::multi_agents::parse_thread_id(&receiver.thread_id)
                else {
                    continue;
                };
                self.upsert_agent_picker_thread(
                    thread_id,
                    receiver.agent_nickname.clone(),
                    receiver.agent_role.clone(),
                    /*is_closed*/ false,
                );
            }
        }

        let Some(receiver_thread_ids) = collab_receiver_thread_ids(notification) else {
            return;
        };

        for receiver_thread_id in receiver_thread_ids {
            if collab_receiver_is_not_found(notification, receiver_thread_id) {
                continue;
            }

            let Ok(thread_id) = ThreadId::from_string(receiver_thread_id) else {
                tracing::warn!(
                    thread_id = receiver_thread_id,
                    "ignoring collab receiver with invalid thread id during local caching"
                );
                continue;
            };

            if self.agent_navigation.get(&thread_id).is_some() {
                continue;
            }

            self.upsert_agent_picker_thread(
                thread_id, /*agent_nickname*/ None, /*agent_role*/ None,
                /*is_closed*/ false,
            );
        }
    }

    async fn infer_session_for_started_thread(
        &self,
        thread_id: ThreadId,
        notification: &ThreadStartedNotification,
    ) -> Option<ThreadSessionState> {
        let mut session = self.primary_session_configured.clone()?;
        session.thread_id = thread_id;
        session.thread_name = notification.thread.name.clone();
        session.model_provider_id = notification.thread.model_provider.clone();
        session
            .set_cwd_retargeting_implicit_runtime_workspace_root(notification.thread.cwd.clone());
        let rollout_path = notification.thread.path.clone();
        let saved_model_settings =
            read_saved_model_settings(self.state_db.as_deref(), thread_id, rollout_path.as_deref())
                .await;
        if let Some(model) = saved_model_settings.model {
            session.model = model;
        } else if rollout_path.is_some() {
            session.model.clear();
        }
        if saved_model_settings.reasoning_effort.is_some() || rollout_path.is_some() {
            session.reasoning_effort = saved_model_settings.reasoning_effort;
        }
        session.message_history = None;
        session.rollout_path = rollout_path;
        Some(session)
    }

    pub(super) async fn enqueue_thread_request(
        &mut self,
        thread_id: ThreadId,
        request: ServerRequest,
    ) -> Result<()> {
        let inactive_interactive_request = if self.active_thread_id != Some(thread_id) {
            self.interactive_request_for_thread_request(thread_id, &request)
                .await?
        } else {
            None
        };
        let (sender, store) = {
            let channel = self.ensure_thread_channel(thread_id);
            (channel.sender.clone(), Arc::clone(&channel.store))
        };

        let (should_send, pending_status) = {
            let mut guard = store.lock().await;
            guard.push_request(request.clone());
            (guard.active, guard.side_parent_pending_status())
        };
        let request_status = SideParentStatus::for_request(&request);

        if should_send {
            match sender.try_send(ThreadBufferedEvent::Request(Box::new(request))) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    tokio::spawn(async move {
                        if let Err(err) = sender.send(event).await {
                            tracing::warn!("thread {thread_id} event channel closed: {err}");
                        }
                    });
                }
                Err(TrySendError::Closed(_)) => {
                    tracing::warn!("thread {thread_id} event channel closed");
                }
            }
        } else if self.active_side_parent_thread_id().is_none()
            && let Some(request) = inactive_interactive_request
        {
            self.push_thread_interactive_request(request);
        }
        if let Some(status) = pending_status.or(request_status) {
            self.set_side_parent_status(thread_id, Some(status));
        }
        self.refresh_pending_thread_approvals().await;
        Ok(())
    }

    pub(super) async fn enqueue_thread_history_entry_response(
        &mut self,
        thread_id: ThreadId,
        event: HistoryLookupResponse,
    ) -> Result<()> {
        let (sender, store) = {
            let channel = self.ensure_thread_channel(thread_id);
            (channel.sender.clone(), Arc::clone(&channel.store))
        };

        let should_send = {
            let mut guard = store.lock().await;
            let should_send = guard.active;
            // Active batch responses remain queued in the receiver across a concurrent detach, so
            // retaining another deep copy for thread replay only accumulates already-delivered
            // history data. Inactive responses still need the buffer because they are not sent.
            if !should_send || !matches!(&event, HistoryLookupResponse::Batch { .. }) {
                guard.push_buffered_event(ThreadBufferedEvent::HistoryEntryResponse(event.clone()));
            }
            should_send
        };

        if should_send {
            match sender.try_send(ThreadBufferedEvent::HistoryEntryResponse(event)) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    tokio::spawn(async move {
                        if let Err(err) = sender.send(event).await {
                            tracing::warn!("thread {thread_id} event channel closed: {err}");
                        }
                    });
                }
                Err(TrySendError::Closed(_)) => {
                    tracing::warn!("thread {thread_id} event channel closed");
                }
            }
        }
        Ok(())
    }

    pub(super) async fn enqueue_primary_thread_session(
        &mut self,
        session: ThreadSessionState,
        turns: Vec<Turn>,
    ) -> Result<()> {
        self.enqueue_primary_thread_session_with_presentation(
            session,
            turns,
            ThreadAttachPresentation::SessionLineage,
        )
        .await
    }

    pub(super) async fn enqueue_primary_thread_session_with_presentation(
        &mut self,
        session: ThreadSessionState,
        turns: Vec<Turn>,
        presentation: ThreadAttachPresentation,
    ) -> Result<()> {
        if let Err(err) = self
            .config
            .permissions
            .approval_policy
            .set(session.approval_policy.to_core())
        {
            tracing::warn!(%err, "failed to sync app approval policy from thread session");
        }
        if let Err(err) = self
            .config
            .permissions
            .set_permission_profile_from_session_snapshot(
                PermissionProfileSnapshot::from_session_snapshot(
                    session.permission_profile.clone(),
                    session.active_permission_profile.clone(),
                ),
            )
        {
            tracing::warn!(%err, "failed to sync app permissions from thread session");
        }
        self.config.approvals_reviewer = session.approvals_reviewer;

        let thread_id = session.thread_id;
        if self.primary_thread_id != Some(thread_id) {
            self.recap.reset_for_new_thread(Instant::now());
        }
        self.primary_thread_id = Some(thread_id);
        self.agents_overview.threads.entry(thread_id).or_default();
        self.primary_session_configured = Some(session.clone());
        self.upsert_agent_picker_thread(
            thread_id, /*agent_nickname*/ None, /*agent_role*/ None,
            /*is_closed*/ false,
        );
        let channel = self.ensure_thread_channel(thread_id);
        {
            let mut store = channel.store.lock().await;
            store.set_session(session.clone(), turns.clone());
        }
        self.activate_thread_channel(thread_id).await;
        self.chat_widget
            .set_initial_user_message_submit_suppressed(/*suppressed*/ true);
        if !turns.is_empty() {
            self.chat_widget.set_token_info(/*info*/ None);
        }
        match presentation {
            ThreadAttachPresentation::SessionLineage => {
                self.chat_widget.handle_thread_session(session);
            }
            ThreadAttachPresentation::PromptEdit => {
                self.chat_widget.handle_prompt_edit_thread_session(session);
            }
        }
        let should_buffer_initial_replay = !turns.is_empty();
        if should_buffer_initial_replay {
            self.app_event_tx
                .send(AppEvent::BeginInitialHistoryReplayBuffer);
        }
        let now = Instant::now();

        self.recap.seed_from_turns(&turns, now);
        self.schedule_recap_check(thread_id, now);

        self.chat_widget
            .replay_thread_turns(turns, ReplayKind::ResumeInitialMessages);
        if should_buffer_initial_replay {
            self.app_event_tx
                .send(AppEvent::EndInitialHistoryReplayBuffer);
        }
        if matches!(presentation, ThreadAttachPresentation::PromptEdit) {
            self.chat_widget.emit_prompt_edit_thread_event();
        }
        let pending = std::mem::take(&mut self.pending_primary_events);
        for pending_event in pending {
            match pending_event {
                ThreadBufferedEvent::Notification(notification) => {
                    self.enqueue_thread_notification(thread_id, *notification)
                        .await?;
                }
                ThreadBufferedEvent::Request(request) => {
                    self.enqueue_thread_request(thread_id, *request).await?;
                }
                ThreadBufferedEvent::HistoryEntryResponse(event) => {
                    self.enqueue_thread_history_entry_response(thread_id, event)
                        .await?;
                }
                ThreadBufferedEvent::McpInventoryResult(event) => {
                    self.handle_mcp_inventory_result(
                        /*thread_id*/ None,
                        event.request_seq,
                        event.result,
                        event.detail,
                    )
                    .await;
                }
                ThreadBufferedEvent::FeedbackSubmission(event) => {
                    self.enqueue_thread_feedback_event(thread_id, event).await;
                }
            }
        }
        self.chat_widget
            .set_initial_user_message_submit_suppressed(/*suppressed*/ false);
        self.chat_widget.submit_initial_user_message_if_pending();
        Ok(())
    }

    pub(super) async fn enqueue_primary_thread_notification(
        &mut self,
        notification: ServerNotification,
    ) -> Result<()> {
        if let Some(thread_id) = self.primary_thread_id {
            return self
                .enqueue_thread_notification(thread_id, notification)
                .await;
        }
        self.pending_primary_events
            .push_back(ThreadBufferedEvent::Notification(Box::new(notification)));
        Ok(())
    }

    pub(super) async fn enqueue_primary_thread_request(
        &mut self,
        request: ServerRequest,
    ) -> Result<()> {
        if let Some(thread_id) = self.primary_thread_id {
            return self.enqueue_thread_request(thread_id, request).await;
        }
        self.pending_primary_events
            .push_back(ThreadBufferedEvent::Request(Box::new(request)));
        Ok(())
    }

    pub(super) async fn refresh_snapshot_session_if_needed(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        is_replay_only: bool,
        snapshot: &mut ThreadEventSnapshot,
    ) {
        if !self.should_refresh_snapshot_session(thread_id, snapshot) {
            return;
        }

        if is_replay_only {
            match app_server
                .thread_read(thread_id, /*include_turns*/ false)
                .await
            {
                Ok(thread) => {
                    let session = self.session_state_for_thread_read(thread_id, &thread).await;
                    if let Some(channel) = self.thread_event_channels.get(&thread_id) {
                        channel.store.lock().await.session = Some(session.clone());
                    }
                    snapshot.session = Some(session);
                }
                Err(err) => {
                    tracing::warn!(
                        thread_id = %thread_id,
                        error = %err,
                        "failed to refresh replay-only thread session before replay"
                    );
                }
            }
            return;
        }

        match app_server
            .resume_thread(
                self.config.clone(),
                thread_id,
                crate::app_server_session::ResumeModelSettings::PreserveExistingThread,
            )
            .await
        {
            Ok(started) => {
                self.apply_refreshed_snapshot_thread(thread_id, started, snapshot)
                    .await
            }
            Err(err) => {
                tracing::warn!(
                    thread_id = %thread_id,
                    error = %err,
                    "failed to refresh inferred thread session before replay"
                );
            }
        }
    }

    pub(super) fn should_refresh_snapshot_session(
        &self,
        thread_id: ThreadId,
        snapshot: &ThreadEventSnapshot,
    ) -> bool {
        !self.side_threads.contains_key(&thread_id)
            && snapshot.session.as_ref().is_none_or(|session| {
                session.model.trim().is_empty()
                    || session.reasoning_effort.is_none()
                    || session.rollout_path.is_none()
            })
    }

    pub(super) async fn apply_refreshed_snapshot_thread(
        &mut self,
        thread_id: ThreadId,
        started: AppServerStartedThread,
        snapshot: &mut ThreadEventSnapshot,
    ) {
        let AppServerStartedThread { session, turns, .. } = started;
        if let Some(channel) = self.thread_event_channels.get(&thread_id) {
            let mut store = channel.store.lock().await;
            store.set_session(session.clone(), turns.clone());
            store.rebase_buffer_after_session_refresh();
        }
        snapshot.session = Some(session);
        snapshot.turns = turns;
        snapshot
            .events
            .retain(ThreadEventStore::event_survives_session_refresh);
    }

    pub(super) async fn handle_backtrack_thread_rollback_response(
        &mut self,
        thread_id: ThreadId,
        response: &ThreadRollbackResponse,
    ) {
        let is_active = self.active_thread_id == Some(thread_id);
        let rollout_path = response.thread.path.clone();
        if is_active {
            self.chat_widget.update_rollout_path(rollout_path.clone());
        }
        if self.primary_thread_id == Some(thread_id)
            && let Some(session) = self.primary_session_configured.as_mut()
        {
            session.rollout_path = rollout_path.clone();
        }
        let old_receiver = if is_active {
            self.active_thread_rx.take()
        } else {
            self.thread_event_channels
                .get_mut(&thread_id)
                .and_then(|channel| channel.receiver.take())
        };
        let mut retained_events = Vec::new();
        if let Some(mut receiver) = old_receiver {
            loop {
                match receiver.try_recv() {
                    Ok(event) if ThreadEventStore::event_survives_thread_rollback(&event) => {
                        retained_events.push(event);
                    }
                    Ok(_) => {}
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            }
        }

        let store = self
            .thread_event_channels
            .get(&thread_id)
            .map(|channel| Arc::clone(&channel.store));
        if let Some(store) = store {
            let mut store = store.lock().await;
            if let Some(session) = store.session.as_mut() {
                session.rollout_path = rollout_path;
            }
            store.apply_thread_rollback(response);
        }

        let replacement_receiver = self
            .thread_event_channels
            .get_mut(&thread_id)
            .map(|channel| {
                let (sender, receiver) = mpsc::channel(channel.sender.max_capacity());
                channel.sender = sender;
                channel.receiver = None;
                for event in retained_events {
                    if let Err(err) = channel.sender.try_send(event) {
                        tracing::warn!(
                            "failed to retain thread {thread_id} event after rollback: {err}"
                        );
                    }
                }
                receiver
            });
        if let Some(receiver) = replacement_receiver {
            if is_active {
                self.active_thread_rx = Some(receiver);
            } else if let Some(channel) = self.thread_event_channels.get_mut(&thread_id) {
                channel.receiver = Some(receiver);
            }
        } else if is_active {
            self.clear_active_thread().await;
        }
        self.handle_backtrack_rollback_succeeded();
    }

    /// Opens the `/subagents` picker after refreshing cached labels for known threads.
    ///
    /// The picker state is derived from long-lived thread channels plus best-effort metadata
    /// refreshes from the backend. Refresh failures are treated as "thread is only inspectable by
    /// historical id now" and converted into closed picker entries instead of deleting them, so
    /// the stable traversal order remains intact for review and keyboard navigation.
    pub(super) async fn drain_active_thread_events(&mut self, tui: &mut tui::Tui) -> Result<()> {
        let frame_deadline = Instant::now() + tui::TARGET_FRAME_INTERVAL;
        self.drain_active_thread_events_until(tui, frame_deadline)
            .await
    }

    pub(super) async fn drain_active_thread_events_until(
        &mut self,
        tui: &mut tui::Tui,
        frame_deadline: Instant,
    ) -> Result<()> {
        let Some(mut rx) = self.active_thread_rx.take() else {
            return Ok(());
        };

        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(event) => {
                    self.handle_thread_event_now_recovering_file_changes(event)
                        .await
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
            if Instant::now() >= frame_deadline {
                break;
            }
        }

        if !disconnected {
            self.active_thread_rx = Some(rx);
        } else {
            self.clear_active_thread().await;
        }

        if self.backtrack_render_pending {
            tui.frame_requester().schedule_frame();
        }
        Ok(())
    }

    /// Returns `(closed_thread_id, primary_thread_id)` when a non-primary active
    /// thread has died and we should fail over to the primary thread.
    ///
    /// A user-requested shutdown (`ExitMode::ShutdownFirst`) sets
    /// `pending_shutdown_exit_thread_id`; matching shutdown completions are ignored
    /// here so Ctrl+C-like exits don't accidentally resurrect the main thread.
    ///
    /// Failover is only eligible when all of these are true:
    /// 1. the event is `thread/closed`;
    /// 2. the active thread differs from the primary thread;
    /// 3. the active thread is not the pending shutdown-exit thread.
    pub(super) fn active_non_primary_shutdown_target(
        &self,
        notification: &ServerNotification,
    ) -> Option<(ThreadId, ThreadId)> {
        if !matches!(notification, ServerNotification::ThreadClosed(_)) {
            return None;
        }
        let active_thread_id = self.active_thread_id?;
        let primary_thread_id = self.primary_thread_id?;
        if self.pending_shutdown_exit_thread_id == Some(active_thread_id) {
            return None;
        }
        (active_thread_id != primary_thread_id).then_some((active_thread_id, primary_thread_id))
    }

    #[cfg(test)]
    pub(super) fn replay_thread_snapshot(
        &mut self,
        mut snapshot: ThreadEventSnapshot,
        resume_restored_queue: bool,
    ) {
        self.replay_thread_snapshot_with_in_flight_state(
            snapshot,
            resume_restored_queue,
            /*preserve_in_flight_turn*/ true,
        );
    }

    pub(super) fn replay_thread_snapshot_with_in_flight_state(
        &mut self,
        snapshot: ThreadEventSnapshot,
        resume_restored_queue: bool,
        preserve_in_flight_turn: bool,
    ) {
        let request_changes = snapshot
            .events
            .iter()
            .map(|event| {
                let ThreadBufferedEvent::Request(request) = event else {
                    return None;
                };
                let ServerRequest::FileChangeRequestApproval { params, .. } = request.as_ref()
                else {
                    return None;
                };
                file_change_changes(
                    snapshot.events.iter(),
                    &snapshot.turns,
                    &params.turn_id,
                    &params.item_id,
                )
            })
            .collect::<Vec<_>>();
        self.refresh_mcp_startup_expected_servers_from_config();
        let should_buffer_replay = !snapshot.turns.is_empty() || !snapshot.events.is_empty();
        if should_buffer_replay {
            self.app_event_tx
                .send(AppEvent::BeginThreadSwitchHistoryReplayBuffer);
        }
        let suppress_replay_notices =
            replay_filter::snapshot_has_pending_interactive_request(&snapshot);
        let ThreadEventSnapshot {
            session,
            turns,
            events,
            mut input_state,
            active_turn_timing,
        } = snapshot;
        // Attaching the incoming session can refresh session-dependent surfaces that attempt to
        // drain the current queue. Suppress autosend before that boundary so queued input from the
        // outgoing thread cannot be submitted under the incoming thread before its saved input
        // state is restored.
        self.chat_widget
            .set_queue_autosend_suppressed(/*suppressed*/ true);
        if let Some(session) = session {
            if session.reasoning_effort != Some(ReasoningEffortConfig::Ultra) {
                self.chat_widget
                    .set_plan_mode_reasoning_effort(self.config.plan_mode_reasoning_effort.clone());
            }
            if self.side_threads.contains_key(&session.thread_id) {
                self.chat_widget.handle_side_thread_session(session);
            } else if suppress_replay_notices {
                self.chat_widget.handle_thread_session_quiet(session);
            } else {
                self.chat_widget.handle_thread_session(session);
            }
        }
        let recovered_input = snapshot
            .input_state
            .as_ref()
            .is_some_and(|input| input.recovered_queue)
            .then(|| snapshot.input_state.take())
            .flatten();
        self.sync_mcp_inventory_loading_for_current_thread();
        let restored_active_turn_timing = if preserve_in_flight_turn {
            active_turn_timing
        } else {
            None
        };
        if let Some(input_state) = input_state.as_mut() {
            input_state.set_active_turn_timing(restored_active_turn_timing.clone());
        }
        self.chat_widget.restore_thread_input_state(
            input_state,
            ThreadInputStateRestoreMode {
                preserve_in_flight_turn,
            },
        );
        if let Some((turn_id, started_at)) = restored_active_turn_timing.as_ref() {
            self.chat_widget.restore_active_turn(turn_id, *started_at);
        }
        let preserves_live_processes =
            preserve_in_flight_turn && restored_active_turn_timing.is_some();
        let replay_kind = if preserves_live_processes {
            ReplayKind::ThreadSnapshot
        } else {
            ReplayKind::ReplayOnlyThreadSnapshot
        };
        if !turns.is_empty() {
            self.chat_widget.replay_thread_turns(turns, replay_kind);
        }
        for (event, changes) in events.into_iter().zip(request_changes) {
            if suppress_replay_notices && replay_filter::event_is_notice(&event) {
                continue;
            }
            match (event, changes) {
                (ThreadBufferedEvent::Request(request), Some(changes)) => {
                    self.handle_file_change_request(*request, changes)
                }
                (event, _) => self.handle_thread_event_replay(event, replay_kind),
            }
        }
        if let Some((turn_id, started_at)) = restored_active_turn_timing {
            self.chat_widget
                .restore_replayed_turn_origin(&turn_id, started_at);
        }
        if !preserves_live_processes {
            // Closed and replay-only threads cannot own process-local terminals. Keep persisted
            // command items in the transcript, but discard any `InProgress` item state rebuilt
            // while replaying saved turns or buffered events.
            self.chat_widget.finalize_replayed_process_tracking();
        }
        if should_buffer_replay {
            self.app_event_tx
                .send(AppEvent::EndInitialHistoryReplayBuffer);
        }
        if recovered_input.is_some() {
            self.chat_widget.restore_reconnected_input(recovered_input);
        }
        self.chat_widget
            .set_queue_autosend_suppressed(/*suppressed*/ false);
        self.chat_widget
            .set_initial_user_message_submit_suppressed(/*suppressed*/ false);
        self.chat_widget.submit_initial_user_message_if_pending();
        if resume_restored_queue {
            self.chat_widget.maybe_send_next_queued_input();
        }
        self.refresh_status_line();
    }

    pub(super) fn should_wait_for_initial_session(session_selection: &SessionSelection) -> bool {
        matches!(
            session_selection,
            SessionSelection::StartFresh | SessionSelection::Exit
        )
    }

    pub(super) fn should_prompt_for_paused_goal_after_startup_resume(
        session_selection: &SessionSelection,
        initial_prompt: &Option<String>,
        initial_images: &[PathBuf],
    ) -> bool {
        matches!(session_selection, SessionSelection::Resume(_))
            && initial_prompt.is_none()
            && initial_images.is_empty()
    }

    pub(super) fn should_handle_active_thread_events(
        waiting_for_initial_session_configured: bool,
        has_active_thread_receiver: bool,
    ) -> bool {
        has_active_thread_receiver && !waiting_for_initial_session_configured
    }

    pub(super) fn should_stop_waiting_for_initial_session(
        waiting_for_initial_session_configured: bool,
        primary_thread_id: Option<ThreadId>,
    ) -> bool {
        waiting_for_initial_session_configured && primary_thread_id.is_some()
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn handle_skills_list_response(&mut self, response: SkillsListResponse) {
        let cwd = self.chat_widget.config_ref().cwd.clone();
        let errors = errors_for_cwd(&cwd, &response);
        let errors = self.skill_load_warnings.newly_active_errors(&errors);
        emit_skill_load_warnings(&self.app_event_tx, &errors);
        self.chat_widget.handle_skills_list_response(response);
    }

    fn startup_request_may_open_protected_view(&self, request: &ServerRequest) -> bool {
        let ServerRequest::McpServerElicitationRequest { request_id, params } = request else {
            return true;
        };

        match &params.request {
            codex_app_server_protocol::McpServerElicitationRequest::Form { .. } => true,
            codex_app_server_protocol::McpServerElicitationRequest::OpenAiForm { .. }
            | codex_app_server_protocol::McpServerElicitationRequest::OpenAiElicitationForm {
                ..
            } => false,
            request @ codex_app_server_protocol::McpServerElicitationRequest::Url { .. } => {
                let thread_id = ThreadId::from_string(&params.thread_id)
                    .unwrap_or_else(|_| self.chat_widget.thread_id().unwrap_or_default());
                AppLinkViewParams::from_url_app_server_request(
                    thread_id,
                    &params.server_name,
                    request_id.clone(),
                    request,
                )
                .is_some()
            }
        }
    }

    pub(super) fn handle_thread_event_now(&mut self, event: ThreadBufferedEvent) {
        let needs_refresh = matches!(
            &event,
            ThreadBufferedEvent::Notification(notification)
                if matches!(
                    notification.as_ref(),
                    ServerNotification::TurnStarted(_)
                        | ServerNotification::ThreadTokenUsageUpdated(_)
                )
        );
        match event {
            ThreadBufferedEvent::Notification(notification) => {
                self.cache_collab_receiver_threads_for_notification(notification.as_ref());
                self.chat_widget
                    .handle_server_notification(*notification, /*replay_kind*/ None);
            }
            ThreadBufferedEvent::Request(request) => {
                if self
                    .pending_app_server_requests
                    .contains_server_request(request.as_ref())
                {
                    let may_open_protected_view =
                        self.startup_request_may_open_protected_view(request.as_ref());
                    let received_at = self
                        .pending_app_server_requests
                        .request_received_at(request.as_ref())
                        .unwrap_or_else(std::time::Instant::now);
                    self.chat_widget.handle_server_request(
                        *request,
                        /*replay_kind*/ None,
                        received_at,
                    );
                    if may_open_protected_view
                        && self.startup_protected_input_boundary
                        && !self.chat_widget.has_active_view()
                    {
                        self.startup_pending_protected_request = true;
                    }
                }
            }
            ThreadBufferedEvent::HistoryEntryResponse(event) => {
                self.chat_widget.handle_history_entry_response(event);
            }
            ThreadBufferedEvent::McpInventoryResult(event) => {
                if event.request_seq.is_some()
                    && event.request_seq
                        != self.chat_widget.thread_id().and_then(|thread_id| {
                            self.latest_mcp_inventory_request_seq
                                .get(&thread_id)
                                .copied()
                        })
                {
                    return;
                }
                self.chat_widget.clear_mcp_inventory_loading();
                self.clear_committed_mcp_inventory_loading();
                let config = self.chat_widget.config_ref().clone();
                match event.result {
                    Ok(statuses) => {
                        self.chat_widget
                            .sync_mcp_server_name_completions_from_statuses(&statuses);
                        if config.mcp_servers.get().is_empty() && statuses.is_empty() {
                            self.chat_widget
                                .add_to_history(history_cell::empty_mcp_output());
                        } else {
                            self.chat_widget.add_to_history(
                                history_cell::new_mcp_tools_output_from_statuses(
                                    &statuses,
                                    event.detail,
                                ),
                            );
                        }
                    }
                    Err(err) => self
                        .chat_widget
                        .add_error_message(format!("Failed to load MCP inventory: {err}")),
                }
            }
            ThreadBufferedEvent::FeedbackSubmission(event) => {
                self.handle_feedback_thread_event(event);
            }
        }
        if needs_refresh {
            self.refresh_status_line();
        }
    }

    pub(super) fn handle_thread_event_replay(
        &mut self,
        event: ThreadBufferedEvent,
        replay_kind: ReplayKind,
    ) {
        match event {
            ThreadBufferedEvent::Notification(notification) => self
                .chat_widget
                .handle_server_notification(*notification, Some(replay_kind)),
            ThreadBufferedEvent::Request(request) => {
                let may_open_protected_view =
                    self.startup_request_may_open_protected_view(request.as_ref());
                let received_at = self
                    .pending_app_server_requests
                    .request_received_at(request.as_ref())
                    .unwrap_or_else(std::time::Instant::now);
                self.chat_widget
                    .handle_server_request(*request, Some(replay_kind), received_at);
                if may_open_protected_view
                    && self.startup_protected_input_boundary
                    && !self.chat_widget.has_active_view()
                {
                    self.startup_pending_protected_request = true;
                }
            }
            ThreadBufferedEvent::HistoryEntryResponse(event) => {
                self.chat_widget.handle_history_entry_response(event)
            }
            ThreadBufferedEvent::McpInventoryResult(event) => {
                if event.request_seq.is_some()
                    && event.request_seq
                        != self.chat_widget.thread_id().and_then(|thread_id| {
                            self.latest_mcp_inventory_request_seq
                                .get(&thread_id)
                                .copied()
                        })
                {
                    return;
                }
                let config = self.chat_widget.config_ref().clone();
                match event.result {
                    Ok(statuses) => {
                        self.chat_widget
                            .sync_mcp_server_name_completions_from_statuses(&statuses);
                        if config.mcp_servers.get().is_empty() && statuses.is_empty() {
                            self.chat_widget
                                .add_to_history(history_cell::empty_mcp_output());
                        } else {
                            self.chat_widget.add_to_history(
                                history_cell::new_mcp_tools_output_from_statuses(
                                    &statuses,
                                    event.detail,
                                ),
                            );
                        }
                    }
                    Err(err) => self
                        .chat_widget
                        .add_error_message(format!("Failed to load MCP inventory: {err}")),
                }
            }
            ThreadBufferedEvent::FeedbackSubmission(event) => {
                self.handle_feedback_thread_event(event);
            }
        }
    }

    /// Handles an event emitted by the currently active thread.
    ///
    /// This function enforces shutdown intent routing: unexpected non-primary
    /// thread shutdowns fail over to the primary thread, while user-requested
    /// app exits consume only the tracked shutdown completion and then proceed.
    pub(super) async fn handle_active_thread_event(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        event: ThreadBufferedEvent,
    ) -> Result<()> {
        // Capture this before any potential thread switch: we only want to clear
        // the exit marker when the currently active thread acknowledges shutdown.
        let pending_shutdown_exit_completed = matches!(
            &event,
            ThreadBufferedEvent::Notification(notification)
                if matches!(notification.as_ref(), ServerNotification::ThreadClosed(_))
        ) && self.pending_shutdown_exit_thread_id
            == self.active_thread_id;

        // Processing order matters:
        //
        // 1. handle unexpected non-primary shutdown failover first;
        // 2. clear pending exit marker for matching shutdown;
        // 3. forward the event through normal handling.
        //
        // This preserves the mental model that user-requested exits do not trigger
        // failover, while true sub-agent deaths still do.
        if let ThreadBufferedEvent::Notification(notification) = &event
            && let Some((closed_thread_id, primary_thread_id)) =
                self.active_non_primary_shutdown_target(notification.as_ref())
        {
            self.mark_agent_picker_thread_closed(closed_thread_id);
            if self.side_threads.contains_key(&closed_thread_id) {
                self.discard_closed_side_thread(closed_thread_id).await;
                self.select_agent_thread(tui, app_server, primary_thread_id)
                    .await?;
            } else {
                self.select_agent_thread_and_discard_side(tui, app_server, primary_thread_id)
                    .await?;
            }
            if self.active_thread_id == Some(primary_thread_id) {
                self.chat_widget.add_info_message(
                    format!(
                        "Agent thread {closed_thread_id} closed. Switched back to main thread."
                    ),
                    /*hint*/ None,
                );
            } else {
                self.clear_active_thread().await;
                self.chat_widget.add_error_message(format!(
                    "Agent thread {closed_thread_id} closed. Failed to switch back to main thread {primary_thread_id}.",
                ));
            }
            return Ok(());
        }

        if pending_shutdown_exit_completed {
            // Clear only after seeing the shutdown completion for the tracked
            // thread, so unrelated shutdowns cannot consume this marker.
            self.pending_shutdown_exit_thread_id = None;
        }
        let had_active_view = self.chat_widget.has_active_view();
        self.handle_thread_event_now_recovering_file_changes(event)
            .await;
        if !had_active_view
            && self.chat_widget.has_active_view()
            && self.startup_protected_input_boundary
        {
            self.chat_widget.pre_draw_tick();
            self.render_chat_widget_frame(tui, tui.terminal.last_known_screen_size)?;
            tui.discard_pending_input_before_interactive_screen()?;
            self.startup_pending_protected_request = false;
        }
        if self.backtrack_render_pending {
            tui.frame_requester().schedule_frame();
        }
        Ok(())
    }
}

fn agent_role_selector(role: &str) -> String {
    let reserved = role == "new"
        || role.eq_ignore_ascii_case(codex_protocol::MAIN_AGENT_NICKNAME)
        || is_agent_target_action(role);
    let requires_namespace = reserved
        || role.is_empty()
        || role.chars().all(|ch| ch.is_ascii_digit())
        || role.chars().any(char::is_whitespace)
        || role.chars().any(|ch| matches!(ch, '"' | '\\'))
        || ThreadId::from_string(role).is_ok()
        || [
            "fork:", "w:", "model:", "effort:", "id:", "ref:", "nick:", "role:",
        ]
        .iter()
        .any(|prefix| role.starts_with(prefix));
    if !requires_namespace {
        return role.to_string();
    }

    let role = role.replace('\\', "\\\\").replace('"', "\\\"");
    format!("role:\"{role}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::ActivePermissionProfile;
    use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_WORKSPACE;

    async fn config_with_workspace_profile() -> Config {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        ConfigBuilder::default()
            .codex_home(temp_dir.path().to_path_buf())
            .harness_overrides(ConfigOverrides {
                default_permissions: Some(BUILT_IN_PERMISSION_PROFILE_WORKSPACE.to_string()),
                ..ConfigOverrides::default()
            })
            .build()
            .await
            .expect("config should build")
    }

    #[tokio::test]
    async fn turn_permissions_use_active_profile_when_available() {
        let config = config_with_workspace_profile().await;
        let active_permission_profile = config.permissions.active_permission_profile();

        assert_eq!(
            App::turn_permissions_override_from_config(
                &config,
                active_permission_profile.as_ref(),
                /*runtime_permission_profile_override*/ None,
            ),
            TurnPermissionsOverride::ActiveProfile(ActivePermissionProfile::new(
                BUILT_IN_PERMISSION_PROFILE_WORKSPACE
            ))
        );
    }

    #[tokio::test]
    async fn turn_permissions_preserve_server_snapshot_without_local_override() {
        let mut config = config_with_workspace_profile().await;
        config
            .permissions
            .set_permission_profile(PermissionProfile::read_only())
            .expect("read-only profile should be allowed");

        assert_eq!(
            App::turn_permissions_override_from_config(
                &config, /*active_permission_profile*/ None,
                /*runtime_permission_profile_override*/ None,
            ),
            TurnPermissionsOverride::Preserve
        );
    }

    #[tokio::test]
    async fn turn_permissions_send_legacy_sandbox_for_local_override() {
        let mut config = config_with_workspace_profile().await;
        let permission_profile = PermissionProfile::workspace_write();
        config
            .permissions
            .set_permission_profile(permission_profile.clone())
            .expect("workspace profile should be allowed");
        let effective_permission_profile = config.permissions.effective_permission_profile();

        assert_eq!(
            App::turn_permissions_override_from_config(
                &config,
                /*active_permission_profile*/ None,
                Some(&permission_profile),
            ),
            TurnPermissionsOverride::LegacySandbox(effective_permission_profile)
        );
    }

    #[test]
    fn role_autocomplete_uses_bare_names_unless_they_need_a_namespace() {
        assert_eq!(agent_role_selector("reviewer"), "reviewer");
        assert_eq!(agent_role_selector("main"), "role:\"main\"");
        assert_eq!(agent_role_selector("MAIN"), "role:\"MAIN\"");
        assert_eq!(agent_role_selector("queue"), "role:\"queue\"");
        assert_eq!(agent_role_selector("2"), "role:\"2\"");
        assert_eq!(agent_role_selector("model:custom"), "role:\"model:custom\"");
        assert_eq!(agent_role_selector("effort:high"), "role:\"effort:high\"");
        assert_eq!(
            agent_role_selector("Ada \"reviewer\""),
            "role:\"Ada \\\"reviewer\\\"\""
        );
        assert_eq!(
            agent_role_selector("019faa07-aa3d-78d3-9eca-66cd8626adad"),
            "role:\"019faa07-aa3d-78d3-9eca-66cd8626adad\""
        );
    }
}
