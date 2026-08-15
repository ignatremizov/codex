//! Root-scoped background refresh for the agent picker.

use super::agent_navigation::AgentPickerThreadVisibility;
use super::app_server_event_targets::ServerNotificationThreadTarget;
use super::app_server_event_targets::server_notification_thread_target;
use super::*;
use crate::app_event::AgentPickerRefresh;
use crate::app_server_session::load_agent_aliases;
use crate::app_server_session::load_agent_queued_turns;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadStatus;
use std::collections::HashSet;

pub(super) const AGENT_PICKER_VIEW_ID: &str = "agent-picker";
const AGENT_PICKER_PAGE_SIZE: u32 = 100;
const AGENT_PICKER_MAX_THREADS: usize = 1_000;

impl App {
    pub(super) fn apply_primary_agent_aliases(
        &mut self,
        aliases: Vec<codex_app_server_protocol::AgentAlias>,
    ) {
        let rows = aliases.clone();
        self.agent_navigation.replace_aliases(aliases);
        for alias in rows {
            let Ok(thread_id) = ThreadId::from_string(&alias.thread_id) else {
                continue;
            };
            let is_known_open = self.current_displayed_thread_id() == Some(thread_id)
                || self
                    .thread_event_channels
                    .get(&thread_id)
                    .is_some_and(|channel| channel.attachment() == ThreadEventAttachment::Live)
                || self
                    .agent_navigation
                    .get(&thread_id)
                    .is_some_and(|entry| !entry.is_closed);
            let is_closed =
                alias.state != codex_app_server_protocol::AgentAliasState::Active || !is_known_open;
            self.upsert_agent_picker_thread_without_sync(
                thread_id,
                alias.nickname,
                /*agent_role*/ None,
                is_closed,
            );
        }
        self.agent_navigation.order_by_agent_ref();
        self.sync_active_agent_label();
    }

    pub(super) async fn refresh_primary_agent_aliases(&mut self, app_server: &AppServerSession) {
        let Some(primary_thread_id) = self.primary_thread_id else {
            return;
        };
        match app_server.agent_aliases(primary_thread_id).await {
            Ok(aliases) => self.apply_primary_agent_aliases(aliases),
            Err(err) => {
                tracing::warn!(%err, "failed to refresh durable agent aliases");
            }
        }
        self.refresh_primary_agent_queue(app_server).await;
    }

    pub(super) fn refresh_agent_picker_threads(
        &mut self,
        app_server: &AppServerSession,
        root: ThreadId,
    ) {
        let Some(request_id) = self.agent_navigation.begin_picker_refresh(root) else {
            return;
        };
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = async {
                let aliases = load_agent_aliases(&request_handle, root)
                    .await
                    .map_err(|err| err.to_string())?;
                let queued = load_agent_queued_turns(&request_handle, root)
                    .await
                    .map_err(|err| err.to_string())?;
                let agent_root_thread_id = aliases
                    .iter()
                    .find(|alias| {
                        alias.agent_ref == "1"
                            && alias.state
                                != codex_app_server_protocol::AgentAliasState::Transferred
                    })
                    .and_then(|alias| ThreadId::from_string(&alias.thread_id).ok())
                    .unwrap_or(root);
                let mut refresh = AgentPickerRefresh {
                    aliases,
                    queued,
                    ..Default::default()
                };
                for archived in [false, true] {
                    let mut cursor = None;
                    let mut seen_cursors = HashSet::new();
                    let mut listed = 0;
                    while listed < AGENT_PICKER_MAX_THREADS && seen_cursors.insert(cursor.clone()) {
                        let page = match request_handle
                            .request_typed::<ThreadListResponse>(ClientRequest::ThreadList {
                                request_id: RequestId::String(Uuid::new_v4().to_string()),
                                params: ThreadListParams {
                                    excluded_thread_ids: None,
                                    attached_to_thread_id: None,
                                    originators: None,
                                    cursor,
                                    limit: Some(AGENT_PICKER_PAGE_SIZE),
                                    sort_key: None,
                                    sort_direction: Some(SortDirection::Desc),
                                    model_providers: Some(vec![]),
                                    source_kinds: None,
                                    archived: Some(archived),
                                    section_id: None,
                                    project_id: None,
                                    cwd: None,
                                    use_state_db_only: true,
                                    search_term: None,
                                    parent_thread_id: None,
                                    ancestor_thread_id: Some(agent_root_thread_id.to_string()),
                                },
                            })
                            .await
                        {
                            Ok(page) => page,
                            Err(err) if !archived && listed == 0 => return Err(err.to_string()),
                            Err(err) => {
                                tracing::warn!(%err, archived, "incomplete agent picker refresh");
                                break;
                            }
                        };
                        let page_threads: Vec<_> = page
                            .data
                            .into_iter()
                            .take(AGENT_PICKER_MAX_THREADS - listed)
                            .collect();
                        listed += page_threads.len();
                        if archived {
                            refresh.archived_thread_ids.extend(
                                page_threads
                                    .into_iter()
                                    .filter_map(|thread| ThreadId::from_string(&thread.id).ok()),
                            );
                        } else {
                            refresh.threads.extend(page_threads);
                        }
                        let Some(next_cursor) = page.next_cursor else {
                            break;
                        };
                        cursor = Some(next_cursor);
                    }
                }
                refresh.threads.reverse();
                match request_handle
                    .request_typed::<ThreadReadResponse>(ClientRequest::ThreadRead {
                        request_id: RequestId::String(Uuid::new_v4().to_string()),
                        params: ThreadReadParams {
                            thread_id: agent_root_thread_id.to_string(),
                            include_turns: false,
                        },
                    })
                    .await
                {
                    Ok(response)
                        if !refresh
                            .threads
                            .iter()
                            .any(|thread| thread.id == response.thread.id) =>
                    {
                        refresh.threads.insert(0, response.thread);
                    }
                    Ok(_) => {}
                    Err(err) => {
                        tracing::warn!(
                            %err,
                            thread_id = %agent_root_thread_id,
                            "failed to refresh agent picker root metadata"
                        );
                    }
                }
                Ok(refresh)
            }
            .await;

            app_event_tx.send(AppEvent::AgentPickerThreadsLoaded {
                primary_thread_id: root,
                request_id,
                result,
            });
        });
    }

    pub(super) fn apply_agent_picker_thread_refresh(
        &mut self,
        app_server: &AppServerSession,
        root: ThreadId,
        request_id: Uuid,
        result: Result<AgentPickerRefresh, String>,
    ) {
        let Some(completion) = self
            .agent_navigation
            .finish_picker_refresh(root, request_id)
        else {
            return;
        };
        if self.primary_thread_id != Some(root) {
            return;
        }
        let refresh = match result {
            Ok(refresh) => refresh,
            Err(err) => {
                tracing::warn!(%err, "failed to refresh agent picker descendants");
                self.agent_navigation.prune_untracked_picker_exclusions();
                if completion.follow_up_requested {
                    self.refresh_agent_picker_threads(app_server, root);
                }
                return;
            }
        };
        let selected_thread_id = self.selected_agent_picker_thread_id();
        self.apply_primary_agent_aliases(refresh.aliases);
        self.apply_primary_agent_queue(refresh.queued);
        for thread in refresh.threads {
            let Ok(thread_id) = ThreadId::from_string(&thread.id) else {
                continue;
            };
            if !completion.visibility_changed_threads.contains(&thread_id) {
                self.update_agent_picker_thread_visibility(
                    thread_id,
                    AgentPickerThreadVisibility::Visible,
                );
            }
            let live = self
                .thread_event_channels
                .get(&thread_id)
                .is_some_and(|channel| channel.attachment() == ThreadEventAttachment::Live);
            let previous = self.agent_navigation.get(&thread_id);
            let alias = self.agent_navigation.alias(thread_id);
            let is_running = matches!(thread.status, ThreadStatus::Active { .. });
            let update_liveness = previous.is_none() || !is_running;
            let has_active_alias = alias.is_some_and(|alias| {
                alias.state == codex_app_server_protocol::AgentAliasState::Active
            });
            let is_closed = alias.is_some_and(|alias| {
                alias.state != codex_app_server_protocol::AgentAliasState::Active
            }) || !live && matches!(thread.status, ThreadStatus::NotLoaded);
            if !is_closed && previous.is_some_and(|entry| entry.is_closed) && !has_active_alias {
                continue;
            }
            let agent_path = crate::app_server_session::source_agent_path(&thread.source);
            let parent_thread_id = crate::app_server_session::thread_parent_thread_id(&thread);
            let agent_nickname = self.agent_navigation.authoritative_nickname(
                thread_id,
                thread
                    .agent_nickname
                    .or_else(|| previous.and_then(|entry| entry.agent_nickname.clone())),
            );
            let agent_role = thread
                .agent_role
                .or_else(|| previous.and_then(|entry| entry.agent_role.clone()));
            if matches!(
                thread.source,
                codex_app_server_protocol::SessionSource::SubAgent(_)
            ) {
                self.agent_navigation.mark_subagent(thread_id);
            }
            self.upsert_agent_picker_thread(thread_id, agent_nickname, agent_role, is_closed);
            self.agent_navigation.set_agent_path(thread_id, agent_path);
            self.agent_navigation
                .set_parent_thread_id(thread_id, parent_thread_id);
            if !live && update_liveness {
                self.agent_navigation.set_running(thread_id, is_running);
            }
        }
        for thread_id in refresh.archived_thread_ids {
            if !completion.visibility_changed_threads.contains(&thread_id) {
                self.update_agent_picker_thread_visibility(
                    thread_id,
                    AgentPickerThreadVisibility::Hidden,
                );
            }
        }
        self.agent_navigation.prune_untracked_picker_exclusions();
        self.agent_navigation.order_by_agent_ref();

        self.replace_agent_picker_view(selected_thread_id);
        if completion.follow_up_requested {
            self.refresh_agent_picker_threads(app_server, root);
        }
    }

    pub(super) fn set_agent_picker_thread_visibility(
        &mut self,
        thread_id: ThreadId,
        visibility: AgentPickerThreadVisibility,
    ) {
        let selected_thread_id = self.selected_agent_picker_thread_id();
        self.update_agent_picker_thread_visibility(thread_id, visibility);
        self.replace_agent_picker_view(selected_thread_id);
    }

    fn update_agent_picker_thread_visibility(
        &mut self,
        thread_id: ThreadId,
        visibility: AgentPickerThreadVisibility,
    ) {
        if matches!(visibility, AgentPickerThreadVisibility::Hidden) {
            self.agent_navigation.mark_stopped(thread_id);
            if let Some(channel) = self.thread_event_channels.get_mut(&thread_id) {
                channel.mark_replay_only();
            }
        }
        self.agent_navigation
            .set_picker_thread_visibility(thread_id, visibility);
    }

    pub(super) fn handle_agent_picker_visibility_notification(
        &mut self,
        app_server: &AppServerSession,
        notification: &ServerNotification,
    ) {
        let visibility = match notification {
            ServerNotification::ThreadArchived(_) => AgentPickerThreadVisibility::Hidden,
            ServerNotification::ThreadUnarchived(_) => AgentPickerThreadVisibility::Visible,
            _ => return,
        };
        let ServerNotificationThreadTarget::Thread(thread_id) =
            server_notification_thread_target(notification)
        else {
            return;
        };
        let refresh_uncached_thread = matches!(visibility, AgentPickerThreadVisibility::Visible)
            && self.agent_navigation.get(&thread_id).is_none();
        self.set_agent_picker_thread_visibility(thread_id, visibility);
        if refresh_uncached_thread
            && let Some(primary_thread_id) = self.primary_thread_id
            && !self
                .agent_navigation
                .queue_picker_refresh(primary_thread_id)
        {
            self.refresh_agent_picker_threads(app_server, primary_thread_id);
        }
    }

    fn selected_agent_picker_thread_id(&self) -> Option<ThreadId> {
        let selected = self
            .chat_widget
            .selected_index_for_present_view(AGENT_PICKER_VIEW_ID)?;
        self.agent_navigation
            .visible_threads()
            .get(selected)
            .map(|(thread_id, _)| *thread_id)
    }

    fn replace_agent_picker_view(&mut self, selected_thread_id: Option<ThreadId>) {
        let selected = selected_thread_id.and_then(|selected_thread_id| {
            self.agent_navigation
                .visible_threads()
                .iter()
                .position(|(thread_id, _)| *thread_id == selected_thread_id)
        });
        let params = self.agent_picker_selection_view_params(selected);
        self.chat_widget
            .replace_selection_view_if_present(AGENT_PICKER_VIEW_ID, params);
    }
}
