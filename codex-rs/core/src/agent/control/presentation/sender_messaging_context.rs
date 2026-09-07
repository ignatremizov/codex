//! Sender-only request projection. Captures are disposable reads of live authority,
//! not a second policy registry; only lifecycle/policy refresh delivers notices.

use super::messaging_context::PermissionNotice;
use super::messaging_context::identity_label;
use super::*;

enum NoticeScope {
    Subtree,
    Directed,
}

struct SenderNotice {
    scope: NoticeScope,
    thread_id: ThreadId,
    mode: TargetMessageRouteMode,
}

pub(super) struct SenderMessagingContext {
    notices: Vec<SenderNotice>,
    has_exceptions: bool,
    pub(super) allowed_targets: HashSet<ThreadId>,
}

impl SenderMessagingContext {
    /// The same precedence drives delivered notices and request-only projection.
    pub(super) fn derive(
        sender: SessionPresentationId,
        active_turn: Option<&str>,
        recipients: impl Iterator<Item = SessionPresentationId>,
        parents: &HashMap<ThreadId, ThreadId>,
        policies: &HashMap<SessionPresentationId, (u64, TargetMessageRouteMode)>,
        relationships: &HashMap<
            (SessionPresentationId, SessionPresentationId),
            ResponseObserverRelationship,
        >,
    ) -> Self {
        let mut context = Self {
            notices: policies
                .iter()
                .filter(|(scope, _)| is_descendant(sender.thread_id, scope.thread_id, parents))
                .map(|(scope, (_, mode))| SenderNotice {
                    scope: NoticeScope::Subtree,
                    thread_id: scope.thread_id,
                    mode: *mode,
                })
                .collect(),
            has_exceptions: relationships.iter().any(|((_, target), relationship)| {
                *target == sender && relationship.reply_route.is_some()
            }),
            allowed_targets: HashSet::new(),
        };
        for recipient in recipients {
            if sender == recipient {
                continue;
            }
            // Supervisor task dispatch is independent of lateral/reverse permissions.
            if is_descendant(recipient.thread_id, sender.thread_id, parents) {
                context.allowed_targets.insert(recipient.thread_id);
                continue;
            }
            let relationship = relationships.get(&(recipient, sender));
            let explicit = relationship.and_then(|relationship| relationship.reply_route);
            let mode = explicit
                .or_else(|| subtree_mode(sender.thread_id, recipient.thread_id, parents, policies));
            if mode == Some(TargetMessageRouteMode::Enabled)
                || (mode.is_none()
                    && active_turn.is_some_and(|turn_id| {
                        relationship.is_some_and(|relationship| {
                            relationship
                                .turns
                                .get(turn_id)
                                .is_some_and(|turn| turn.target_messages)
                                || relationship
                                    .pending_next_turn
                                    .as_ref()
                                    .is_some_and(|turn| turn.target_messages)
                                || relationship
                                    .pending_admissions
                                    .values()
                                    .any(|turn| turn.target_messages)
                        })
                    }))
            {
                context.allowed_targets.insert(recipient.thread_id);
            }
            if let Some(mode) = explicit {
                context.notices.push(SenderNotice {
                    scope: NoticeScope::Directed,
                    thread_id: recipient.thread_id,
                    mode,
                });
            }
        }
        context
    }

    pub(super) async fn render_notices(
        &self,
        control: &AgentControl,
    ) -> CodexResult<Vec<PermissionNotice>> {
        let mut notices = Vec::with_capacity(self.notices.len());
        for notice in &self.notices {
            let identity = control
                .model_visible_agent_identity_for_version(MultiAgentVersion::V1, notice.thread_id)
                .await?;
            let action = match notice.mode {
                TargetMessageRouteMode::Enabled => "enabled",
                TargetMessageRouteMode::Disabled => "disabled",
            };
            let (key, text) = match notice.scope {
                NoticeScope::Subtree => {
                    let label = match &identity {
                        crate::context::AgentContextIdentity::V1 { nickname, .. }
                            if nickname.as_deref() == Some("Main") =>
                        {
                            "Main".to_string()
                        }
                        _ => identity_label(&identity),
                    };
                    let exceptions = if self.has_exceptions {
                        " Explicit directed settings still apply."
                    } else {
                        ""
                    };
                    (
                        format!("subtree.{}", notice.thread_id),
                        format!(
                            "User {action} send_input within {label}'s subtree, including sibling communication and future agents.{exceptions}"
                        ),
                    )
                }
                NoticeScope::Directed => {
                    let label = identity_label(&identity);
                    (
                        format!("directed.{}", notice.thread_id),
                        format!("User {action} send_input to {label}."),
                    )
                }
            };
            notices.push(PermissionNotice { key, text });
        }
        notices.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(notices)
    }
}

#[derive(PartialEq, Eq)]
struct SenderAuthority {
    policies: HashMap<SessionPresentationId, (u64, TargetMessageRouteMode)>,
    continuity: HashMap<SessionPresentationId, (u64, Option<ThreadId>)>,
    relationships:
        HashMap<(SessionPresentationId, SessionPresentationId), ResponseObserverRelationship>,
}

impl SenderAuthority {
    fn capture(state: &PresentationState, sender: SessionPresentationId) -> Self {
        Self {
            policies: state.subtree_messaging.clone(),
            continuity: state
                .live_revert_messaging
                .iter()
                .map(|(source, marker)| (*source, (marker.generation, marker.parent_thread_id)))
                .collect(),
            relationships: state
                .response_observation_by_observer_child
                .iter()
                .filter(|((_, source), _)| *source == sender)
                .map(|(pair, relationship)| (*pair, relationship.clone()))
                .collect(),
        }
    }
}

pub(super) fn has_messaging_authority<'a>(
    policies: &HashMap<SessionPresentationId, (u64, TargetMessageRouteMode)>,
    mut relationships: impl Iterator<Item = &'a ResponseObserverRelationship>,
) -> bool {
    !policies.is_empty()
        || relationships.any(|relationship| {
            relationship.reply_route.is_some()
                || relationship.turns.values().any(|turn| turn.target_messages)
                || relationship
                    .pending_next_turn
                    .as_ref()
                    .is_some_and(|turn| turn.target_messages)
                || relationship
                    .pending_admissions
                    .values()
                    .any(|turn| turn.target_messages)
        })
}

impl AgentControl {
    /// Read only the requesting sender's context. Neither historical hints nor
    /// delivery bookkeeping can restore grants or trigger delivery to any runtime.
    pub(crate) async fn messaging_context_snapshot(
        &self,
        sender: SessionPresentationId,
    ) -> CodexResult<MessagingContextSnapshot> {
        loop {
            let authority =
                SenderAuthority::capture(&self.wait_agent_presentations.state(), sender);
            // Ordinary/unbound sessions must not initialize graph ownership for projection.
            if !has_messaging_authority(&authority.policies, authority.relationships.values()) {
                return Ok(MessagingContextSnapshot::default());
            }
            let manager = self.upgrade()?;
            let Ok(sender_thread) = manager.get_thread_including_pending(sender.thread_id).await
            else {
                return Ok(MessagingContextSnapshot::default());
            };
            if sender_thread.session.presentation_id() != sender
                || sender_thread.multi_agent_version() == Some(MultiAgentVersion::V2)
                || !sender_thread
                    .session
                    .services
                    .agent_control
                    .matches_session_id(self.session_id())
            {
                return Ok(MessagingContextSnapshot::default());
            }
            let active_turn = sender_thread.session.active_agent_response_turn_id();
            let generations: HashMap<_, _> = authority
                .policies
                .keys()
                .chain(authority.continuity.keys())
                .map(|source| {
                    (
                        source.thread_id,
                        manager.agent_lifecycle_generation(source.thread_id),
                    )
                })
                .collect();
            let mut ids: HashSet<_> = manager.list_thread_ids().await.into_iter().collect();
            ids.insert(sender.thread_id);
            ids.extend(authority.continuity.keys().map(|source| source.thread_id));
            let mut parents = HashMap::new();
            let mut published = HashMap::new();
            let mut runtimes = HashMap::new();
            for id in ids {
                if let Ok(thread) = manager.get_thread(id).await {
                    published.insert(id, thread.session.presentation_id());
                }
                if let Ok(thread) = manager.get_thread_including_pending(id).await
                    && thread
                        .session
                        .services
                        .agent_control
                        .matches_session_id(self.session_id())
                {
                    if let Some(parent) = thread.session_source.parent_thread_id() {
                        parents.insert(id, parent);
                    }
                    runtimes.insert(id, thread.session.presentation_id());
                }
            }
            let continuity_is_current = |source: SessionPresentationId, generation| {
                messaging_continuity_is_current(
                    source,
                    generation,
                    generations[&source.thread_id],
                    published.get(&source.thread_id).copied(),
                )
            };
            for (source, (generation, parent)) in &authority.continuity {
                if continuity_is_current(*source, *generation)
                    && runtimes.get(&source.thread_id) != Some(source)
                {
                    if let Some(parent) = parent {
                        parents.insert(source.thread_id, *parent);
                    } else {
                        parents.remove(&source.thread_id);
                    }
                }
            }
            // Filter this disposable view only. Pruning shared authority and clearing
            // wake reservations belong to refresh, never sampling/compaction.
            let policies = authority
                .policies
                .iter()
                .filter(|(root, (generation, _))| {
                    continuity_is_current(**root, *generation)
                        && (runtimes.get(&root.thread_id) == Some(*root)
                            || authority
                                .continuity
                                .get(*root)
                                .is_some_and(|(marked, _)| marked == generation))
                })
                .map(|(root, policy)| (*root, *policy))
                .collect();
            let context = SenderMessagingContext::derive(
                sender,
                active_turn.as_deref(),
                runtimes.values().copied(),
                &parents,
                &policies,
                &authority.relationships,
            );
            let notices = context.render_notices(self).await?;
            let history = sender_thread.session.clone_history().await;
            let state = self.wait_agent_presentations.state();
            // A live revert can rekey authority while runtime/identity reads await.
            // Retry without changing shared policy, ancestry or delivery bookkeeping.
            if SenderAuthority::capture(&state, sender) != authority
                || sender_thread.session.active_agent_response_turn_id() != active_turn
                || generations
                    .iter()
                    .any(|(id, generation)| manager.agent_lifecycle_generation(*id) != *generation)
            {
                continue;
            }
            let notices = notices
                .into_iter()
                .map(|notice| {
                    let pending = state
                        .pending_messaging_context
                        .get(&(sender, notice.key.clone()))
                        .filter(|(turn, _)| *turn == active_turn)
                        .map(|(_, item)| item);
                    let existing = history
                        .raw_items()
                        .rev()
                        .find(|item| notice_parts(item).is_some_and(|(key, _)| key == notice.key));
                    existing
                        .filter(|item| {
                            notice_parts(item).is_some_and(|(_, text)| text == notice.text)
                        })
                        .or_else(|| {
                            pending.filter(|item| {
                                notice_parts(item).is_some_and(|(_, text)| text == notice.text)
                            })
                        })
                        .cloned()
                        // An undelivered notice is deterministic and model-only. Delivery
                        // later assigns its durable identity; requests never invent IDs.
                        .unwrap_or_else(|| ContextualUserFragment::into(notice))
                })
                .collect();
            return Ok(MessagingContextSnapshot {
                notices,
                allowed_targets: context.allowed_targets,
            });
        }
    }
}

#[cfg(test)]
#[path = "sender_messaging_context_tests.rs"]
mod tests;
