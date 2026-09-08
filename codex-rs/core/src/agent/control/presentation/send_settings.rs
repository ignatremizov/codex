//! Projects authoritative UUID settings into existing runtime messaging policy.
//! Never restores subscriptions, turn bindings, or consumed wake reservations.

use super::*;
use codex_agent_graph_store::AgentSendMode;
use codex_agent_graph_store::AgentSendScope;
use codex_agent_graph_store::AgentSendSetting;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;

struct StoredSendSettings {
    parents: HashMap<ThreadId, ThreadId>,
    settings: Vec<AgentSendSetting>,
}

#[cfg(test)]
#[path = "send_settings_tests.rs"]
mod tests;

impl AgentControl {
    pub(in crate::agent::control) fn install_persisted_send_mode(
        &self,
        recipient: SessionPresentationId,
        sender: SessionPresentationId,
        mode: TargetMessageRouteMode,
    ) -> ResponseObserverRelationship {
        let mut state = self.wait_agent_presentations.state();
        let relationship = state
            .response_observation_by_observer_child
            .entry((recipient, sender))
            .or_default();
        relationship.reply_route = Some(mode);
        relationship.reply_route_from_settings = true;
        if mode == TargetMessageRouteMode::Disabled {
            for observation in relationship
                .pending_next_turn
                .iter_mut()
                .chain(relationship.pending_admissions.values_mut())
                .chain(relationship.turns.values_mut())
            {
                observation.message_wake_reservation_id = None;
                observation.message_wake_turn_id = None;
            }
        }
        let relationship = relationship.clone();
        drop(state);
        self.wait_agent_presentations
            .response_observation_changed
            .notify_waiters();
        relationship
    }

    /// Called under the messaging permission mutex; acquires no additional runtime locks.
    pub(in crate::agent::control) async fn persist_agent_send_setting_locked(
        &self,
        scope: AgentSendScope,
        mode: TargetMessageRouteMode,
    ) -> CodexResult<()> {
        let store = self
            .upgrade()?
            .agent_graph_store()
            .filter(|store| store.supports_agent_send_settings())
            .ok_or_else(|| {
                CodexErr::UnsupportedOperation(
                    "this graph store cannot persist user messaging settings".into(),
                )
            })?;
        store
            .replace_agent_send_setting(
                scope,
                match mode {
                    TargetMessageRouteMode::Enabled => AgentSendMode::Enabled,
                    TargetMessageRouteMode::Disabled => AgentSendMode::Disabled,
                },
            )
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!("failed to persist messaging setting: {error}"))
            })?;
        Ok(())
    }

    async fn read_persisted_send_settings(
        &self,
        sender: ThreadId,
        recipient: ThreadId,
    ) -> CodexResult<Option<StoredSendSettings>> {
        let manager = self.upgrade()?;
        let Some(store) = manager
            .agent_graph_store()
            .filter(|store| store.supports_agent_send_settings())
        else {
            return Ok(None);
        };
        let mut parents = HashMap::new();
        let mut ancestors = HashSet::new();
        for endpoint in [sender, recipient] {
            let mut current = endpoint;
            while ancestors.insert(current) {
                let parent = store
                    .find_thread_spawn_parent(current)
                    .await
                    .map_err(|error| {
                        CodexErr::Fatal(format!("failed to read messaging ancestry: {error}"))
                    })?;
                let Some(parent) = parent else {
                    break;
                };
                parents.insert(current, parent);
                current = parent;
            }
        }
        let mut scopes: Vec<_> = ancestors
            .iter()
            .map(|id| AgentSendScope::Subtree {
                supervisor_thread_id: *id,
            })
            .collect();
        if sender != recipient {
            scopes.push(AgentSendScope::Directed {
                sender_thread_id: sender,
                receiver_thread_id: recipient,
            });
        }
        let settings = store
            .read_agent_send_settings(scopes)
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!("failed to restore messaging settings: {error}"))
            })?;
        Ok(Some(StoredSendSettings { parents, settings }))
    }

    /// Read-only check under the caller's existing messaging permission transaction.
    ///
    /// Uses only durable explicit settings and downward ancestry. It does not refresh runtime
    /// policy, require a live sender, restore transient grants, or confer lifecycle access.
    /// This is not a cross-root authorization decision or support for transient-mail acceptance.
    pub(crate) async fn mailbox_send_permission_locked(
        &self,
        sender_thread_id: ThreadId,
        receiver_thread_id: ThreadId,
    ) -> CodexResult<bool> {
        let stored = self
            .read_persisted_send_settings(sender_thread_id, receiver_thread_id)
            .await?
            .ok_or_else(|| {
                CodexErr::UnsupportedOperation(
                    "this graph store cannot verify durable mailbox send settings".into(),
                )
            })?;
        if sender_thread_id == receiver_thread_id {
            return Ok(false);
        }
        if codex_protocol::is_agent_descendant(
            receiver_thread_id,
            sender_thread_id,
            &stored.parents,
        ) {
            return Ok(true);
        }
        let mut directed = None;
        let mut policies = HashMap::new();
        for setting in stored.settings {
            match setting.scope {
                AgentSendScope::Directed { .. } => directed = Some(setting.mode),
                AgentSendScope::Subtree {
                    supervisor_thread_id,
                } => {
                    policies.insert(supervisor_thread_id, setting.mode);
                }
            }
        }
        let inherited = codex_protocol::inherited_subtree_policy(
            sender_thread_id,
            receiver_thread_id,
            &stored.parents,
            |id| policies.get(&id).copied(),
        );
        Ok(directed.or(inherited) == Some(AgentSendMode::Enabled))
    }

    /// Warns a loaded sender without submitting input or starting a turn.
    /// The caller has already durably rejected the mail; unloaded senders retain that record.
    pub(crate) async fn publish_mailbox_rejection(
        &self,
        sender_thread_id: ThreadId,
        receiver_thread_id: ThreadId,
        message_id: &str,
        reason: &str,
    ) {
        let Ok(manager) = self.upgrade() else {
            return;
        };
        let Ok(sender) = manager.get_thread(sender_thread_id).await else {
            return;
        };
        sender
            .session
            .send_event_raw(codex_protocol::protocol::Event {
                id: format!("agent-mailbox-rejected-{message_id}"),
                msg: codex_protocol::protocol::EventMsg::Warning(
                    codex_protocol::protocol::WarningEvent {
                message: format!("Mailbox message {message_id} to {receiver_thread_id} was rejected: {reason}"),
                    },
                ),
            })
            .await;
    }

    /// Restores only this pair and its canonical ancestors under the permission mutex.
    /// Legacy backends without this capability retain their existing runtime behavior.
    pub(in crate::agent::control) async fn restore_agent_send_pair_locked(
        &self,
        sender: SessionPresentationId,
        recipient: SessionPresentationId,
    ) -> CodexResult<()> {
        let Some(StoredSendSettings { parents, settings }) = self
            .read_persisted_send_settings(sender.thread_id, recipient.thread_id)
            .await?
        else {
            return Ok(());
        };
        let manager = self.upgrade()?;
        let mut policies = HashMap::new();
        let mut directed = None;
        for setting in settings {
            let mode = match setting.mode {
                AgentSendMode::Enabled => TargetMessageRouteMode::Enabled,
                AgentSendMode::Disabled => TargetMessageRouteMode::Disabled,
            };
            match setting.scope {
                AgentSendScope::Directed { .. } => directed = Some(mode),
                AgentSendScope::Subtree {
                    supervisor_thread_id,
                } => {
                    policies.insert(supervisor_thread_id, mode);
                    let presentation = if supervisor_thread_id == sender.thread_id {
                        Some(sender)
                    } else if supervisor_thread_id == recipient.thread_id {
                        Some(recipient)
                    } else {
                        manager
                            .get_thread_including_pending(supervisor_thread_id)
                            .await
                            .ok()
                            .map(|thread| thread.session.presentation_id())
                    };
                    if let Some(presentation) = presentation {
                        self.wait_agent_presentations
                            .state()
                            .subtree_messaging
                            .insert(
                                presentation,
                                (
                                    manager.agent_lifecycle_generation(supervisor_thread_id),
                                    mode,
                                ),
                            );
                    }
                }
            }
        }
        if sender == recipient {
            return Ok(());
        }
        let inherited = codex_protocol::inherited_subtree_policy(
            sender.thread_id,
            recipient.thread_id,
            &parents,
            |id| policies.get(&id).copied(),
        );
        if let Some(mode) = directed {
            self.install_persisted_send_mode(recipient, sender, mode);
        }
        let mut state = self.wait_agent_presentations.state();
        if let Some(mode) = inherited {
            state
                .inherited_message_routes
                .insert((recipient, sender), mode);
        }
        if directed.or(inherited) == Some(TargetMessageRouteMode::Disabled)
            && let Some(relationship) = state
                .response_observation_by_observer_child
                .get_mut(&(recipient, sender))
        {
            for observation in relationship
                .pending_next_turn
                .iter_mut()
                .chain(relationship.pending_admissions.values_mut())
                .chain(relationship.turns.values_mut())
            {
                observation.message_wake_reservation_id = None;
                observation.message_wake_turn_id = None;
            }
        }
        Ok(())
    }

    /// Restore a pending/current runtime before acquiring an outer observer transaction.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "settings must publish under the same permission mutex as input admission"
    )]
    pub(crate) async fn restore_agent_send_settings(
        &self,
        current: SessionPresentationId,
    ) -> CodexResult<()> {
        let _permission = self.acquire_messaging_permission_transaction().await;
        self.restore_agent_send_settings_locked(current).await
    }

    pub(in crate::agent::control) async fn restore_agent_send_settings_locked(
        &self,
        current: SessionPresentationId,
    ) -> CodexResult<()> {
        self.restore_agent_send_pair_locked(current, current)
            .await?;
        let manager = self.upgrade()?;
        for id in manager.list_thread_ids().await {
            if id == current.thread_id {
                continue;
            }
            let Ok(thread) = manager.get_thread(id).await else {
                continue;
            };
            if !self.matches_session_id(thread.session.services.agent_control.session_id()) {
                continue;
            }
            let peer = thread.session.presentation_id();
            self.restore_agent_send_pair_locked(current, peer).await?;
            self.restore_agent_send_pair_locked(peer, current).await?;
        }
        Ok(())
    }
}
