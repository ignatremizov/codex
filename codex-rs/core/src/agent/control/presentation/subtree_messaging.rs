//! Live subtree defaults. Explicit directed routes remain authoritative; history is not authority.

use super::*;
use crate::context::AgentReplyRoute;
use crate::context::ContextualUserFragment;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::is_agent_descendant as is_descendant;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::MultiAgentVersion;

#[path = "messaging_context.rs"]
mod messaging_context;

#[path = "sender_messaging_context.rs"]
mod sender_messaging_context;

use messaging_context::MessagingContextSnapshot;
use messaging_context::notice_parts;
use sender_messaging_context::SenderMessagingContext;
use sender_messaging_context::has_messaging_authority;

impl LocalAgentControl {
    /// Orders effective permission changes with exact input admission. Acquire after
    /// capacity/mailbox waits and before any response-observation transaction.
    pub(crate) async fn acquire_messaging_permission_transaction(
        &self,
    ) -> tokio::sync::MutexGuard<'_, ()> {
        self.wait_agent_presentations.messaging_refresh.lock().await
    }

    pub(crate) async fn set_subtree_messaging(
        &self,
        root: SessionPresentationId,
        mode: TargetMessageRouteMode,
    ) -> CodexResult<Option<TargetMessageRouteMode>> {
        let control = self.clone();
        tokio::spawn(async move {
            let manager = control.upgrade()?;
            let _lifecycle = manager.acquire_live_agent_lifecycle(root.thread_id).await?;
            control.require_current_agent_ownership(root.thread_id).await?;
            let thread = manager.get_thread(root.thread_id).await?;
            if thread.session.presentation_id() != root {
                return Err(CodexErr::ThreadNotFound(root.thread_id));
            }
            if thread.multi_agent_version() == Some(MultiAgentVersion::V2) {
                return Err(CodexErr::InvalidRequest(
                    "subtree messaging permissions apply to V1; V2 uses native messaging".into(),
                ));
            }
            let _source = manager.agent_turn_queue.acquire_source_admission(root.thread_id).await;
            thread.session.submission_admission.check_ready()?;
            let _admission = thread.session.submission_admission.try_accept_completion_delivery()
                .ok_or_else(|| CodexErr::InvalidRequest("policy source is closing".into()))?;
            let _permission = control.acquire_messaging_permission_transaction().await;
            control.restore_agent_send_pair_locked(root, root).await?;
            let previous = control.wait_agent_presentations.state().subtree_messaging.get(&root).copied();
            let generation = manager.agent_lifecycle_generation(root.thread_id);
            let identity = control.model_visible_agent_identity_for_version(MultiAgentVersion::V1, root.thread_id).await?;
            let label = messaging_context::identity_label(&identity);
            let action = if mode.is_enabled() { "enabled" } else { "disabled" };
            let notice = messaging_context::PermissionNotice {
                key: format!("subtree.{}", root.thread_id),
                text: format!("User {action} send_input within {label}'s subtree, including sibling communication and future agents."),
            };
            control.persist_agent_send_setting_locked(
                codex_agent_graph_store::AgentSendScope::Subtree { supervisor_thread_id: root.thread_id },
                mode,
            ).await?;
            // SQL ACK is the authority boundary; history is a separate audit boundary.
            control.wait_agent_presentations.state().subtree_messaging.insert(root, (generation, mode));
            thread.session.publish_messaging_context(ContextualUserFragment::into(notice), || Ok(()))
                .await.map_err(|error| CodexErr::Fatal(format!(
                    "messaging setting committed but canonical notice failed: {error}; reload before continuing"
                )))?;
            control.refresh_messaging_context_locked(root.thread_id).await.map_err(|error| {
                CodexErr::Fatal(format!("subtree permission committed but context refresh failed: {error}; do not retry"))
            })?;
            Ok(previous.map(|(_, mode)| mode))
        }).await.map_err(|error| CodexErr::Fatal(format!("subtree permission worker lost; do not retry: {error}")))?
    }

    /// Refresh before admission and at turn startup. New children discover peers without a
    /// completion subscription, a model wake, or inheriting a parent's history.
    pub(crate) async fn refresh_subtree_messaging(&self, current: ThreadId) -> CodexResult<()> {
        self.refresh_messaging_context(current).await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "reconciliation must serialize across runtime reads and permission publication"
    )]
    async fn refresh_messaging_context(&self, current: ThreadId) -> CodexResult<()> {
        #[cfg(test)]
        if let Some(attempted) = self
            .wait_agent_presentations
            .messaging_refresh_attempted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let sources = self
                .wait_agent_presentations
                .state()
                .live_revert_messaging
                .keys()
                .copied()
                .collect();
            let _ = attempted.send(sources);
        }
        let _refresh = self.wait_agent_presentations.messaging_refresh.lock().await;
        self.refresh_messaging_context_locked(current).await
    }

    pub(in crate::agent::control) async fn refresh_messaging_context_locked(
        &self,
        current: ThreadId,
    ) -> CodexResult<()> {
        if let Ok(manager) = self.upgrade()
            && let Ok(thread) = manager.get_thread(current).await
        {
            self.restore_agent_send_settings_locked(thread.session.presentation_id())
                .await?;
        }
        {
            let state = self.wait_agent_presentations.state();
            if state.inherited_message_routes.is_empty()
                && !has_messaging_authority(
                    &state.subtree_messaging,
                    state.response_observation_by_observer_child.values(),
                )
            {
                return Ok(());
            }
        }
        let manager = self.upgrade()?;
        let capture_continuity = |state: &PresentationState| {
            state
                .live_revert_messaging
                .iter()
                .map(|(source, marker)| (*source, (marker.generation, marker.parent_thread_id)))
                .collect::<HashMap<_, _>>()
        };
        let (threads, parents, policies, inherited) = loop {
            let (continuity, captured_policies) = {
                let state = self.wait_agent_presentations.state();
                (capture_continuity(&state), state.subtree_messaging.clone())
            };
            let mut ids = manager.list_thread_ids().await;
            if !ids.contains(&current) {
                ids.push(current);
            }
            for source in continuity.keys() {
                if !ids.contains(&source.thread_id) {
                    ids.push(source.thread_id);
                }
            }
            let mut threads = HashMap::new();
            let mut parents = HashMap::new();
            let mut published = HashMap::new();
            for id in ids {
                if let Ok(thread) = manager.get_thread(id).await {
                    published.insert(id, thread.session.presentation_id());
                }
                let Ok(thread) = manager.get_thread(id).await else {
                    continue;
                };
                if let Some(parent) = thread.session_source.parent_thread_id() {
                    parents.insert(id, parent);
                }
                threads.insert(id, thread);
            }
            #[cfg(test)]
            {
                let gate = self
                    .wait_agent_presentations
                    .messaging_refresh_capture_gate
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some((captured, proceed)) = gate {
                    let _ = captured.send(());
                    let _ = proceed.await;
                }
            }
            // Keep the source ancestry while its runtime is absent or awaiting atomic
            // rekey. The marker preserves existing authority, never restored history.
            for (source, (generation, parent)) in &continuity {
                if messaging_continuity_is_current(
                    *source,
                    *generation,
                    manager.agent_lifecycle_generation(source.thread_id),
                    published.get(&source.thread_id).copied(),
                ) && threads
                    .get(&source.thread_id)
                    .is_none_or(|thread| thread.session.presentation_id() != *source)
                {
                    if let Some(parent) = parent {
                        parents.insert(source.thread_id, *parent);
                    } else {
                        parents.remove(&source.thread_id);
                    }
                }
            }
            let policies = {
                let mut state = self.wait_agent_presentations.state();
                // Revert may rekey policy while runtime lookup is suspended, before it
                // waits for this refresh mutex. Never prune that newer policy using the
                // old published-runtime snapshot.
                if state.subtree_messaging != captured_policies
                    || capture_continuity(&state) != continuity
                {
                    continue;
                }
                state
                    .pending_messaging_context
                    .retain(|(sender, _), (turn_id, _)| {
                        threads.get(&sender.thread_id).is_some_and(|thread| {
                            thread.session.presentation_id() == *sender
                                && thread.session.active_agent_response_turn_id() == *turn_id
                        })
                    });
                let continuity = &state.live_revert_messaging;
                let retained_continuity: HashMap<_, _> = continuity
                    .iter()
                    .filter(|(source, marker)| {
                        messaging_continuity_is_current(
                            **source,
                            marker.generation,
                            manager.agent_lifecycle_generation(source.thread_id),
                            published.get(&source.thread_id).copied(),
                        )
                    })
                    .map(|(source, marker)| (*source, marker.generation))
                    .collect();
                state.subtree_messaging.retain(|root, (generation, _)| {
                    messaging_continuity_is_current(
                        *root,
                        *generation,
                        manager.agent_lifecycle_generation(root.thread_id),
                        published.get(&root.thread_id).copied(),
                    ) && (threads
                        .get(&root.thread_id)
                        .is_some_and(|thread| thread.session.presentation_id() == *root)
                        || retained_continuity.get(root) == Some(generation))
                });
                state.subtree_messaging.clone()
            };
            let mut inherited = HashMap::new();
            for sender in threads.values() {
                if sender.multi_agent_version() == Some(MultiAgentVersion::V2) {
                    continue;
                }
                for recipient in threads.values() {
                    if !sender
                        .session
                        .services
                        .agent_control
                        .matches_session_id(recipient.session.services.agent_control.session_id())
                    {
                        continue;
                    }
                    let sender = sender.session.presentation_id();
                    let recipient = recipient.session.presentation_id();
                    if sender == recipient {
                        continue;
                    }
                    if let Some(mode) =
                        subtree_mode(sender.thread_id, recipient.thread_id, &parents, &policies)
                    {
                        inherited.insert((recipient, sender), mode);
                    }
                }
            }
            {
                let mut state = self.wait_agent_presentations.state();
                if state.subtree_messaging != policies || capture_continuity(&state) != continuity {
                    continue;
                }
                state.inherited_message_routes = inherited.clone();
            }
            break (threads, parents, policies, inherited);
        };
        self.wait_agent_presentations
            .response_observation_changed
            .notify_waiters();
        let mut route_modes = inherited.clone();
        for (pair, relationship) in &self
            .wait_agent_presentations
            .state()
            .response_observation_by_observer_child
        {
            if relationship.revoked {
                continue;
            }
            if let Some(mode) = relationship.reply_route {
                route_modes.insert(*pair, mode);
            }
        }
        for ((recipient, sender), mode) in &route_modes {
            if *mode != TargetMessageRouteMode::Enabled
                || self.target_message_route_mode(*recipient, *sender) != Some(*mode)
                || is_descendant(recipient.thread_id, sender.thread_id, &parents)
            {
                continue;
            }
            let Some(sender_thread) = threads.get(&sender.thread_id) else {
                continue;
            };
            if !sender_thread
                .session
                .services
                .agent_control
                .matches_session_id(self.session_id())
            {
                continue;
            }
            let _transaction = self
                .acquire_response_observation_transaction(*recipient)
                .await;
            if self.target_message_route_mode(*recipient, *sender)
                != Some(TargetMessageRouteMode::Enabled)
            {
                continue;
            }
            let history = sender_thread.session.clone_history().await;
            if !history
                .raw_items()
                .any(|item| AgentReplyRoute::persistent_agent_id(item) == Some(recipient.thread_id))
            {
                let item = match self
                    .agent_reply_route_item(sender_thread, *recipient, crate::agent::control::scoped_messages::AgentReplyRouteLifetime::UntilDisabled)
                    .await
                {
                    Ok(item) => item,
                    Err(error) => {
                        tracing::warn!(%error, "peer disappeared during messaging discovery");
                        continue;
                    }
                };
                self.record_messaging_context(
                    &sender_thread.session,
                    format!("route.{}", recipient.thread_id),
                    item,
                )
                .await?;
            }
            self.wait_agent_presentations
                .state()
                .response_observation_by_observer_child
                .entry((*recipient, *sender))
                .or_default()
                .reply_route_context_installed = true;
        }
        let relationships = self
            .wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .clone();
        for sender_thread in threads.values() {
            if sender_thread.multi_agent_version() == Some(MultiAgentVersion::V2)
                || !sender_thread
                    .session
                    .services
                    .agent_control
                    .matches_session_id(self.session_id())
            {
                continue;
            }
            let sender = sender_thread.session.presentation_id();
            let active_turn = sender_thread.session.active_agent_response_turn_id();
            let recipients = threads.values().filter(|thread| {
                thread
                    .session
                    .services
                    .agent_control
                    .matches_session_id(self.session_id())
            });
            let context = SenderMessagingContext::derive(
                sender,
                active_turn.as_deref(),
                recipients.map(|thread| thread.session.presentation_id()),
                &parents,
                &policies,
                &relationships,
            );
            let notices = context.render_notices(self).await?;
            let mut desired_keys: HashSet<_> =
                notices.iter().map(|notice| notice.key.clone()).collect();
            for ((recipient, route_sender), mode) in &route_modes {
                if *route_sender == sender
                    && *mode == TargetMessageRouteMode::Enabled
                    && !is_descendant(recipient.thread_id, sender.thread_id, &parents)
                {
                    desired_keys.insert(format!("route.{}", recipient.thread_id));
                }
            }
            self.wait_agent_presentations
                .state()
                .pending_messaging_context
                .retain(|(pending_sender, key), _| {
                    *pending_sender != sender || desired_keys.contains(key)
                });
            let history = sender_thread.session.clone_history().await;
            for notice in notices {
                let active_turn = sender_thread.session.active_agent_response_turn_id();
                let pending = self
                    .wait_agent_presentations
                    .state()
                    .pending_messaging_context
                    .get(&(sender, notice.key.clone()))
                    .filter(|(turn, _)| *turn == active_turn)
                    .map(|(_, item)| item.clone());
                let existing = history
                    .raw_items()
                    .rev()
                    .find(|item| notice_parts(item).is_some_and(|(key, _)| key == notice.key));
                if let Some(existing) = existing
                    && notice_parts(existing).is_some_and(|(_, text)| text == notice.text)
                    && pending.as_ref().is_none_or(|item| {
                        notice_parts(item).is_some_and(|(_, text)| text == notice.text)
                    })
                {
                    continue;
                }
                let key = notice.key.clone();
                let item = ContextualUserFragment::into(notice);
                self.record_messaging_context(&sender_thread.session, key, item)
                    .await?;
            }
        }
        Ok(())
    }

    async fn record_messaging_context(
        &self,
        sender_session: &Arc<crate::session::session::Session>,
        key: String,
        mut item: ResponseItem,
    ) -> CodexResult<()> {
        let sender = sender_session.presentation_id();
        let turn_id = sender_session.active_agent_response_turn_id();
        {
            let state = self.wait_agent_presentations.state();
            if let Some((pending_turn, pending)) =
                state.pending_messaging_context.get(&(sender, key.clone()))
                && *pending_turn == turn_id
                && matches!((&item, pending),
                    (ResponseItem::Message { content, .. }, ResponseItem::Message { content: pending_content, .. })
                        if content == pending_content)
            {
                return Ok(());
            }
            item.set_id(Some(codex_protocol::ResponseItemId::new("msg")));
            if let Some(active_turn_id) = &turn_id {
                crate::session::session::Session::stamp_response_item_for_history(
                    &mut item,
                    active_turn_id,
                );
            }
        }
        let control = self.clone();
        // The owned recorder publishes policy context without queuing a sampling trigger.
        // Install the cache only with its canonical ACK, including if this waiter is cancelled.
        let cached_item = item.clone();
        sender_session
            .publish_messaging_context(item, move || {
                if turn_id.is_some() {
                    control
                        .wait_agent_presentations
                        .state()
                        .pending_messaging_context
                        .insert((sender, key), (turn_id, cached_item));
                }
                Ok(())
            })
            .await
    }
}

// A pending replacement is not publication: only the public runtime can supersede
// the marked presentation before the revert handoff atomically rekeys its policy.
fn messaging_continuity_is_current(
    source: SessionPresentationId,
    generation: u64,
    current_generation: u64,
    published: Option<SessionPresentationId>,
) -> bool {
    generation == current_generation && published.is_none_or(|runtime| runtime == source)
}

fn subtree_mode(
    sender: ThreadId,
    recipient: ThreadId,
    parents: &HashMap<ThreadId, ThreadId>,
    policies: &HashMap<SessionPresentationId, (u64, TargetMessageRouteMode)>,
) -> Option<TargetMessageRouteMode> {
    codex_protocol::inherited_subtree_policy(sender, recipient, parents, |id| {
        policies
            .iter()
            .find(|(root, _)| root.thread_id == id)
            .map(|(_, (_, mode))| *mode)
    })
}

#[cfg(test)]
#[path = "subtree_messaging_tests.rs"]
mod tests;
