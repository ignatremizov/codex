//! Process-local send policy carried across an explicit live paginated revert.
//!
//! This is a one-use handoff, not history or a second policy registry. Observation state,
//! turn-scoped reply grants, queued work, and wake reservations are deliberately excluded.

use super::*;
use crate::CodexThread;
use codex_protocol::SessionId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::ThreadHistoryMode;

/// Opaque live send-policy handoff for replacement of the same logical thread during revert.
///
/// Not serializable or cloneable: cold resume and forks must not recover live permission from
/// history, and an individual handoff can be consumed only once.
pub struct LiveRevertMessagingSnapshot {
    source: SessionPresentationId,
    source_generation: u64,
    owner: SessionId,
    presentations: Arc<WaitAgentPresentations>,
    source_runtime: Weak<CodexThread>,
    published: bool,
}

#[derive(Clone, Copy)]
pub(super) struct LiveRevertMessagingContinuity {
    pub(super) generation: u64,
    pub(super) parent_thread_id: Option<ThreadId>,
}

impl CodexThread {
    /// Capture current V1 send permissions for the explicit live `thread/revert` operation.
    ///
    /// The caller must serialize user policy changes with its revert/reload boundary. V2 has no
    /// V1 send policy to carry. No conversation history is consulted.
    pub async fn snapshot_agent_messaging_for_revert(
        &self,
    ) -> CodexResult<Option<LiveRevertMessagingSnapshot>> {
        if self.config_snapshot().await.history_mode != ThreadHistoryMode::Paginated {
            return Err(CodexErr::InvalidRequest(
                "live messaging handoff requires a paginated revert".to_string(),
            ));
        }
        if self.multi_agent_version() == Some(MultiAgentVersion::V2) {
            return Ok(None);
        }
        let control = &self.session.services.agent_control;
        let manager = control.upgrade()?;
        let source = self.session.presentation_id();
        let _lifecycle = manager
            .agent_lifecycle_lock(source.thread_id)
            .lock_owned()
            .await;
        let current = manager.get_thread(source.thread_id).await?;
        if current.session.presentation_id() != source {
            return Err(CodexErr::ThreadNotFound(source.thread_id));
        }
        let mut state = control.wait_agent_presentations.state();
        if state.live_revert_messaging.contains_key(&source) {
            return Err(CodexErr::InvalidRequest(
                "a live revert messaging handoff is already active".to_string(),
            ));
        }
        state.live_revert_messaging.insert(
            source,
            LiveRevertMessagingContinuity {
                generation: manager.agent_lifecycle_generation(source.thread_id),
                parent_thread_id: self.session_source.parent_thread_id(),
            },
        );
        Ok(Some(LiveRevertMessagingSnapshot {
            source,
            source_generation: manager.agent_lifecycle_generation(source.thread_id),
            owner: control.session_id(),
            presentations: Arc::clone(&control.wait_agent_presentations),
            source_runtime: Arc::downgrade(&current),
            published: false,
        }))
    }
}

impl LiveRevertMessagingSnapshot {
    /// Rebind policy while the owning resume lifecycle locks still protect the pending runtime.
    pub(crate) async fn restore_before_publication(
        mut self,
        thread: &CodexThread,
    ) -> CodexResult<Self> {
        let control = &thread.session.services.agent_control;
        let replacement = thread.session.presentation_id();
        if replacement.thread_id != self.source.thread_id
            || replacement == self.source
            || control.session_id() != self.owner
            || !Arc::ptr_eq(&self.presentations, &control.wait_agent_presentations)
        {
            return Err(CodexErr::InvalidRequest(
                "live revert messaging handoff must replace the same thread in its current root"
                    .to_string(),
            ));
        }
        let manager = control.upgrade()?;
        if manager.agent_lifecycle_generation(replacement.thread_id) != self.source_generation {
            return Err(CodexErr::InvalidRequest(
                "agent was closed or transferred during live revert; its ended grants cannot be restored"
                    .to_string(),
            ));
        }
        let pending = manager
            .get_thread_including_pending(replacement.thread_id)
            .await?;
        if pending.session.presentation_id() != replacement
            || manager.get_thread(replacement.thread_id).await.is_ok()
        {
            return Err(CodexErr::InvalidRequest(
                "live revert messaging must be restored before runtime publication".to_string(),
            ));
        }
        let endpoints = {
            let state = self.presentations.state();
            state
                .response_observation_by_observer_child
                .keys()
                .filter_map(|&(recipient, sender)| {
                    if recipient == self.source {
                        Some(sender)
                    } else if sender == self.source {
                        Some(recipient)
                    } else {
                        None
                    }
                })
                .collect::<HashSet<_>>()
        };
        let mut current_endpoints = HashSet::new();
        for other in endpoints {
            let Ok(other_thread) = manager.get_thread(other.thread_id).await else {
                continue;
            };
            if other_thread.session.presentation_id() != other
                || other_thread.session.services.agent_control.session_id() != self.owner
            {
                // A separately closed, replaced, or transferred endpoint has ended its grants.
                continue;
            }
            current_endpoints.insert(other);
        }
        {
            let mut state = control.wait_agent_presentations.state();
            let Some(marker) = state.live_revert_messaging.remove(&self.source) else {
                return Err(CodexErr::InvalidRequest(
                    "live revert messaging continuity has ended".to_string(),
                ));
            };
            state.live_revert_messaging.insert(replacement, marker);
            if let Some((generation, mode)) = state.subtree_messaging.remove(&self.source)
                && generation == self.source_generation
            {
                state.subtree_messaging.insert(
                    replacement,
                    (
                        manager.agent_lifecycle_generation(replacement.thread_id),
                        mode,
                    ),
                );
            }
            let directed = state
                .response_observation_by_observer_child
                .iter()
                .filter_map(|(&(recipient, sender), relationship)| {
                    let other = if recipient == self.source {
                        sender
                    } else if sender == self.source {
                        recipient
                    } else {
                        return None;
                    };
                    if !current_endpoints.contains(&other) {
                        return None;
                    }
                    relationship.reply_route.map(|mode| {
                        (
                            if recipient == self.source {
                                replacement
                            } else {
                                recipient
                            },
                            if sender == self.source {
                                replacement
                            } else {
                                sender
                            },
                            mode,
                        )
                    })
                })
                .collect::<Vec<_>>();
            state
                .response_observation_by_observer_child
                .retain(|(recipient, sender), _| {
                    *recipient != self.source && *sender != self.source
                });
            // Rebuild effective inheritance from the authoritative defaults and directed modes.
            state.inherited_message_routes.clear();
            for (recipient, sender, mode) in directed {
                state.response_observation_by_observer_child.insert(
                    (recipient, sender),
                    ResponseObserverRelationship {
                        persistence: ResponseObservationPersistence::Durable,
                        reply_route: Some(mode),
                        ..Default::default()
                    },
                );
            }
            self.source = replacement;
            // A pending replacement is never a reason to retain policy on failed publication.
            self.source_runtime = Weak::new();
        }
        control
            .refresh_subtree_messaging(replacement.thread_id)
            .await?;
        Ok(self)
    }

    pub(crate) fn published(mut self) {
        self.published = true;
    }
}

impl Drop for LiveRevertMessagingSnapshot {
    fn drop(&mut self) {
        let mut state = self.presentations.state();
        if state
            .live_revert_messaging
            .get(&self.source)
            .is_none_or(|marker| marker.generation != self.source_generation)
        {
            return;
        }
        state.live_revert_messaging.remove(&self.source);
        let original_still_running = self.source_runtime.upgrade().is_some_and(|thread| {
            thread.is_running()
                && thread
                    .session
                    .services
                    .agent_control
                    .agent_lifecycle_generation_is_current(
                        self.source.thread_id,
                        self.source_generation,
                    )
        });
        if !self.published && !original_still_running {
            state.subtree_messaging.remove(&self.source);
            state
                .response_observation_by_observer_child
                .retain(|(recipient, sender), _| {
                    *recipient != self.source && *sender != self.source
                });
            state.inherited_message_routes.clear();
            state
                .pending_messaging_context
                .retain(|(sender, _), _| *sender != self.source);
        }
        drop(state);
        self.presentations
            .response_observation_changed
            .notify_waiters();
    }
}
