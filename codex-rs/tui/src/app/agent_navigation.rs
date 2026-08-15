//! Multi-agent picker navigation and labeling state for the TUI app.
//!
//! This module exists to keep the pure parts of multi-agent navigation out of [`crate::app::App`].
//! It owns the stable spawn-order cache used by the `/subagents` picker, keyboard next/previous
//! navigation, and the contextual footer label for the thread currently being watched.
//!
//! Responsibilities here are intentionally narrow:
//! - remember picker entries and their first-seen order
//! - answer traversal questions like "what is the next thread?"
//! - derive user-facing picker/footer text from cached thread metadata
//!
//! Responsibilities that stay in `App`:
//! - discovering threads from the backend
//! - deciding which thread is currently displayed
//! - mutating UI state such as switching threads or updating the footer widget
//!
//! The key invariant is that traversal follows first-seen spawn order rather than thread-id sort
//! order. Once a thread id is observed it keeps its place in the cycle even if the entry is later
//! updated or marked closed.

use super::agent_observation_display::AgentResponseObservationBinding;
use super::agent_observation_display::AgentResponseObservationDisplay;
use super::agent_observation_display::AgentResponseObservationState;
use crate::multi_agents::AgentPickerThreadEntry;
use crate::multi_agents::SubAgentActivityDisplay;
use crate::multi_agents::format_agent_picker_item_name;
use crate::multi_agents::next_agent_shortcut;
use crate::multi_agents::previous_agent_shortcut;
use codex_app_server_protocol::AgentAlias;
use codex_app_server_protocol::AgentAliasState;
use codex_app_server_protocol::AgentFinalResponseHandling;
use codex_app_server_protocol::AgentResponseHandling;
use codex_protocol::MAIN_AGENT_NICKNAME;
use codex_protocol::ThreadId;
use ratatui::text::Span;
use std::collections::HashMap;
use std::collections::HashSet;
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentAliasEntry {
    pub(crate) agent_ref: u64,
    pub(crate) nickname: Option<String>,
    pub(crate) state: AgentAliasState,
}

/// Small state container for multi-agent picker ordering and labeling.
///
/// `App` owns thread lifecycle and UI side effects. This type keeps the pure rules for stable
/// spawn-order traversal, picker copy, and active-agent labels together and separately testable.
///
/// The core invariant is that `order` records first-seen thread ids exactly once, while `threads`
/// stores the latest known metadata for those ids. Mutation is intentionally funneled through
/// `upsert`, `mark_closed`, and `clear` so those two collections do not drift semantically even
/// if they are temporarily out of sync during teardown races.
#[derive(Debug, Default)]
pub(crate) struct AgentNavigationState {
    /// Latest picker metadata for each tracked thread id.
    threads: HashMap<ThreadId, AgentPickerThreadEntry>,
    /// Stable first-seen traversal order for picker rows and keyboard cycling.
    order: Vec<ThreadId>,
    /// Immediate parent for each spawned subagent thread.
    parent_threads: HashMap<ThreadId, ThreadId>,
    /// Threads with observed terminal liveness that must not be revived by delayed activity.
    stopped_threads: HashSet<ThreadId>,
    /// Live response observation keyed by `(observer, target)`.
    response_observations: AgentResponseObservationState,
    /// Source threads holding response handling for the target's next user-authored turn.
    pending_reserved_prompt_sources: HashMap<ThreadId, ThreadId>,
    /// Durable root-scoped identities keyed by canonical thread UUID.
    aliases: HashMap<ThreadId, AgentAliasEntry>,
    /// Coalesces root refreshes while rejecting replies from a previous session.
    pub(super) picker_refresh: Option<(ThreadId, Uuid)>,
}

/// Direction of keyboard traversal through the stable picker order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentNavigationDirection {
    /// Move toward the entry that was seen earlier in spawn order, wrapping at the front.
    Previous,
    /// Move toward the entry that was seen later in spawn order, wrapping at the end.
    Next,
}

impl AgentNavigationState {
    pub(crate) fn begin_picker_refresh(&mut self, thread_id: ThreadId) -> Option<Uuid> {
        if self.picker_refresh.is_some() {
            return None;
        }
        let request_id = Uuid::new_v4();
        self.picker_refresh = Some((thread_id, request_id));
        Some(request_id)
    }

    pub(crate) fn finish_picker_refresh(&mut self, thread_id: ThreadId, request_id: Uuid) -> bool {
        if self.picker_refresh != Some((thread_id, request_id)) {
            return false;
        }
        self.picker_refresh = None;
        true
    }

    /// Returns the cached picker entry for a specific thread id.
    ///
    /// Callers use this when they already know which thread they care about and need the last
    /// metadata captured for picker or footer rendering. If a caller assumes every tracked thread
    /// must be present here, shutdown races can turn that assumption into a panic elsewhere, so
    /// this stays optional.
    pub(crate) fn get(&self, thread_id: &ThreadId) -> Option<&AgentPickerThreadEntry> {
        self.threads.get(thread_id)
    }

    pub(crate) fn is_running(&self, thread_id: ThreadId) -> bool {
        self.threads
            .get(&thread_id)
            .is_some_and(|entry| entry.is_running && !entry.is_closed)
    }

    pub(crate) fn alias(&self, thread_id: ThreadId) -> Option<&AgentAliasEntry> {
        self.aliases.get(&thread_id)
    }

    pub(crate) fn root_thread_id(&self) -> Option<ThreadId> {
        self.aliases.iter().find_map(|(thread_id, alias)| {
            (alias.agent_ref == 1 && alias.state != AgentAliasState::Transferred)
                .then_some(*thread_id)
        })
    }

    pub(crate) fn thread_id_for_ref(&self, agent_ref: u64) -> Option<ThreadId> {
        self.aliases.iter().find_map(|(thread_id, alias)| {
            (alias.agent_ref == agent_ref && alias.state != AgentAliasState::Transferred)
                .then_some(*thread_id)
        })
    }

    pub(crate) fn thread_id_for_nickname(&self, nickname: &str) -> Option<ThreadId> {
        if nickname.eq_ignore_ascii_case(MAIN_AGENT_NICKNAME) {
            return self.root_thread_id();
        }
        self.aliases.iter().find_map(|(thread_id, alias)| {
            (alias.nickname.as_deref() == Some(nickname)
                && alias.state != AgentAliasState::Transferred)
                .then_some(*thread_id)
        })
    }

    pub(crate) fn control_selector(&self, thread_id: ThreadId) -> Option<String> {
        self.aliases.get(&thread_id).map(|alias| {
            if alias.agent_ref == 1 {
                MAIN_AGENT_NICKNAME.to_string()
            } else {
                alias.agent_ref.to_string()
            }
        })
    }

    pub(crate) fn replace_aliases(&mut self, aliases: Vec<AgentAlias>) {
        let aliases = aliases
            .into_iter()
            .filter_map(|alias| {
                let thread_id = ThreadId::from_string(&alias.thread_id).ok()?;
                let agent_ref = alias
                    .agent_ref
                    .parse::<u64>()
                    .ok()
                    .filter(|value| *value > 0)?;
                Some((
                    thread_id,
                    AgentAliasEntry {
                        agent_ref,
                        nickname: alias.nickname,
                        state: alias.state,
                    },
                ))
            })
            .collect::<HashMap<_, _>>();
        for (thread_id, alias) in &aliases {
            if let Some(entry) = self.threads.get_mut(thread_id) {
                // A durable alias is the authoritative user-facing nickname. In particular, an
                // adoption may intentionally omit a colliding historical nickname rather than
                // exposing a label that resolves to another agent.
                entry.agent_nickname.clone_from(&alias.nickname);
            }
        }
        self.aliases = aliases;
    }

    pub(crate) fn upsert_alias(
        &mut self,
        thread_id: ThreadId,
        agent_ref: u64,
        nickname: Option<String>,
        state: AgentAliasState,
    ) {
        if let Some(entry) = self.threads.get_mut(&thread_id) {
            entry.agent_nickname.clone_from(&nickname);
        }
        self.aliases.insert(
            thread_id,
            AgentAliasEntry {
                agent_ref,
                nickname,
                state,
            },
        );
        self.order_by_agent_ref();
    }

    pub(crate) fn authoritative_nickname(
        &self,
        thread_id: ThreadId,
        metadata_nickname: Option<String>,
    ) -> Option<String> {
        self.aliases
            .get(&thread_id)
            .map_or(metadata_nickname, |alias| alias.nickname.clone())
    }

    /// Reconciles cold-discovered row order with durable root-scoped spawn/adoption order.
    pub(crate) fn order_by_agent_ref(&mut self) {
        let previous_positions = self
            .order
            .iter()
            .enumerate()
            .map(|(index, thread_id)| (*thread_id, index))
            .collect::<HashMap<_, _>>();
        let aliases = &self.aliases;
        self.order.sort_by_key(|thread_id| {
            (
                aliases
                    .get(thread_id)
                    .map_or(u64::MAX, |alias| alias.agent_ref),
                previous_positions
                    .get(thread_id)
                    .copied()
                    .unwrap_or(usize::MAX),
            )
        });
    }

    /// Returns whether the picker cache currently knows about any threads.
    ///
    /// This is the cheapest way for `App` to decide whether opening the picker should show "No
    /// agents available yet." rather than constructing picker rows from an empty state.
    pub(crate) fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }

    /// Inserts or updates a picker entry while preserving first-seen traversal order.
    ///
    /// The key invariant of this module is enforced here: a thread id is appended to `order` only
    /// the first time it is seen. Later updates may change nickname, role, or closed state, but
    /// they must not move the thread in the cycle or keyboard navigation would feel unstable.
    /// Missing nickname or role values mean the producer had no update, so they preserve any
    /// identity learned from an earlier thread read or picker refresh.
    pub(crate) fn upsert(
        &mut self,
        thread_id: ThreadId,
        agent_nickname: Option<String>,
        agent_role: Option<String>,
        is_closed: bool,
    ) {
        if !self.threads.contains_key(&thread_id) {
            self.order.push(thread_id);
        }
        let (
            previous_agent_nickname,
            previous_agent_role,
            previous_agent_path,
            previous_is_running,
        ) = self
            .threads
            .get(&thread_id)
            .map(|entry| {
                (
                    entry.agent_nickname.clone(),
                    entry.agent_role.clone(),
                    entry.agent_path.clone(),
                    entry.is_running,
                )
            })
            .unwrap_or((None, None, None, false));
        self.threads.insert(
            thread_id,
            AgentPickerThreadEntry {
                agent_nickname: agent_nickname.or(previous_agent_nickname),
                agent_role: agent_role.or(previous_agent_role),
                agent_path: previous_agent_path,
                is_running: previous_is_running && !is_closed,
                is_closed,
            },
        );
    }

    pub(crate) fn record_sub_agent_activity(&mut self, activity: SubAgentActivityDisplay) {
        if !self.threads.contains_key(&activity.thread_id) {
            self.order.push(activity.thread_id);
        }
        let entry =
            self.threads
                .entry(activity.thread_id)
                .or_insert_with(|| AgentPickerThreadEntry {
                    agent_nickname: None,
                    agent_role: None,
                    agent_path: None,
                    is_running: false,
                    is_closed: false,
                });
        entry.agent_path = Some(activity.agent_path);
        if activity.is_running_hint
            && !entry.is_closed
            && !self.stopped_threads.contains(&activity.thread_id)
        {
            entry.is_running = true;
        } else {
            entry.is_running = false;
            self.stopped_threads.insert(activity.thread_id);
        }
    }

    pub(crate) fn mark_running(&mut self, thread_id: ThreadId) {
        self.response_observations.mark_target_running(thread_id);
        self.pending_reserved_prompt_sources.remove(&thread_id);
        if self
            .threads
            .get(&thread_id)
            .is_some_and(|entry| entry.is_closed)
        {
            return;
        }
        self.stopped_threads.remove(&thread_id);
        self.set_running(thread_id, /*is_running*/ true);
    }

    pub(crate) fn mark_stopped(&mut self, thread_id: ThreadId) {
        self.stopped_threads.insert(thread_id);
        self.set_running(thread_id, /*is_running*/ false);
    }

    pub(crate) fn set_running(&mut self, thread_id: ThreadId, is_running: bool) {
        if !is_running {
            self.response_observations.mark_target_stopped(thread_id);
        }
        if let Some(entry) = self.threads.get_mut(&thread_id) {
            entry.is_running = is_running;
        }
    }

    pub(crate) fn set_agent_path(&mut self, thread_id: ThreadId, agent_path: Option<String>) {
        if let Some(agent_path) = agent_path
            && let Some(entry) = self.threads.get_mut(&thread_id)
        {
            entry.agent_path = Some(agent_path);
        }
    }

    pub(crate) fn set_parent_thread_id(
        &mut self,
        thread_id: ThreadId,
        parent_thread_id: Option<ThreadId>,
    ) {
        match parent_thread_id {
            Some(parent_thread_id) => {
                self.parent_threads.insert(thread_id, parent_thread_id);
            }
            None => {
                self.parent_threads.remove(&thread_id);
            }
        }
    }

    pub(crate) fn parent_thread_id(&self, thread_id: ThreadId) -> Option<ThreadId> {
        self.parent_threads.get(&thread_id).copied()
    }

    pub(crate) fn child_count(&self, thread_id: ThreadId) -> usize {
        self.parent_threads
            .values()
            .filter(|parent_thread_id| **parent_thread_id == thread_id)
            .count()
    }

    pub(crate) fn depth(&self, thread_id: ThreadId) -> usize {
        let mut depth = 0;
        let mut current = thread_id;
        let mut visited = HashSet::new();
        while visited.insert(current) {
            let Some(parent) = self.parent_thread_id(current) else {
                break;
            };
            depth += 1;
            current = parent;
        }
        depth
    }

    pub(crate) fn display_name(
        &self,
        thread_id: ThreadId,
        primary_thread_id: Option<ThreadId>,
    ) -> String {
        let is_primary = primary_thread_id == Some(thread_id);
        self.threads
            .get(&thread_id)
            .map(|entry| {
                if !is_primary
                    && entry.agent_nickname.is_none()
                    && entry.agent_role.is_none()
                    && let Some(agent_path) = entry
                        .agent_path
                        .as_deref()
                        .filter(|agent_path| !agent_path.trim().is_empty())
                {
                    return agent_path.trim().to_string();
                }
                format_agent_picker_item_name(
                    entry.agent_nickname.as_deref(),
                    entry.agent_role.as_deref(),
                    is_primary,
                )
            })
            .unwrap_or_else(|| {
                format_agent_picker_item_name(
                    /*agent_nickname*/ None, /*agent_role*/ None, is_primary,
                )
            })
    }

    pub(crate) fn note_response_observation(
        &mut self,
        observer: ThreadId,
        target: ThreadId,
        binding: AgentResponseObservationBinding,
        response_handling: Option<AgentResponseHandling>,
    ) {
        self.response_observations
            .note(observer, target, binding, response_handling);
    }

    pub(crate) fn replace_user_final_response_observation(
        &mut self,
        observer: ThreadId,
        target: ThreadId,
        binding: AgentResponseObservationBinding,
        final_response: AgentFinalResponseHandling,
    ) {
        self.response_observations.replace_final_response(
            observer,
            target,
            binding,
            final_response,
        );
    }

    #[cfg(test)]
    pub(crate) fn has_wake_subscription(&self, observer: ThreadId, target: ThreadId) -> bool {
        self.response_observations.has_wake(observer, target)
    }

    pub(crate) fn response_observation(
        &self,
        observer: ThreadId,
        target: ThreadId,
    ) -> Option<AgentResponseObservationDisplay> {
        self.response_observations.get(observer, target)
    }

    pub(crate) fn clear_response_observation(&mut self, observer: ThreadId, target: ThreadId) {
        self.response_observations.remove(observer, target);
    }

    pub(crate) fn reserve_prompt_response(&mut self, source: ThreadId, target: ThreadId) {
        self.pending_reserved_prompt_sources.insert(target, source);
    }

    pub(crate) fn reserved_prompt_source(&self, target: ThreadId) -> Option<ThreadId> {
        self.pending_reserved_prompt_sources.get(&target).copied()
    }

    pub(crate) fn clear_reserved_prompt_response(&mut self, target: ThreadId) {
        self.pending_reserved_prompt_sources.remove(&target);
    }

    /// Marks a thread as closed without removing it from the traversal cache.
    ///
    /// Closed threads stay in the picker and in spawn order so users can still review them and so
    /// next/previous navigation does not reshuffle around disappearing entries. If a caller "cleans
    /// this up" by deleting the entry instead, wraparound navigation will silently change shape
    /// mid-session.
    pub(crate) fn mark_closed(&mut self, thread_id: ThreadId) {
        if let Some(entry) = self.threads.get_mut(&thread_id) {
            entry.is_closed = true;
            entry.is_running = false;
        } else {
            self.upsert(
                thread_id, /*agent_nickname*/ None, /*agent_role*/ None,
                /*is_closed*/ true,
            );
        }
        self.response_observations.remove_thread(thread_id);
        self.pending_reserved_prompt_sources.remove(&thread_id);
    }

    /// Drops all cached picker state.
    ///
    /// This is used when `App` tears down thread event state and needs the picker cache to return
    /// to a pristine single-session state.
    pub(crate) fn clear(&mut self) {
        self.threads.clear();
        self.order.clear();
        self.parent_threads.clear();
        self.stopped_threads.clear();
        self.response_observations.clear();
        self.pending_reserved_prompt_sources.clear();
        self.aliases.clear();
        self.picker_refresh = None;
    }

    /// Removes a tracked thread entirely from picker metadata and traversal order.
    ///
    /// This is reserved for entries that were only discovered opportunistically and never became
    /// replayable local threads. Keeping those around after the backend confirms they are gone
    /// would leave ghost rows in `/subagents`.
    pub(crate) fn remove(&mut self, thread_id: ThreadId) {
        self.threads.remove(&thread_id);
        self.order.retain(|candidate| *candidate != thread_id);
        self.parent_threads.remove(&thread_id);
        self.stopped_threads.remove(&thread_id);
        self.response_observations.remove_thread(thread_id);
        self.pending_reserved_prompt_sources.remove(&thread_id);
    }

    /// Returns whether there is at least one tracked thread other than the primary one.
    ///
    /// `App` uses this to decide whether the picker should be available even when the collaboration
    /// feature flag is currently disabled, because already-existing sub-agent threads should remain
    /// inspectable.
    pub(crate) fn has_non_primary_thread(&self, primary_thread_id: Option<ThreadId>) -> bool {
        self.threads
            .keys()
            .any(|thread_id| Some(*thread_id) != primary_thread_id)
    }

    /// Returns live picker rows in the same order users cycle through them.
    ///
    /// The `order` vector is intentionally historical and may briefly contain thread ids that no
    /// longer have cached metadata, so this filters through the map instead of assuming both
    /// collections are perfectly synchronized.
    pub(crate) fn ordered_threads(&self) -> Vec<(ThreadId, &AgentPickerThreadEntry)> {
        self.order
            .iter()
            .filter_map(|thread_id| self.threads.get(thread_id).map(|entry| (*thread_id, entry)))
            .collect()
    }

    pub(crate) fn ordered_path_backed_subagent_threads(
        &self,
        primary_thread_id: Option<ThreadId>,
    ) -> Vec<(ThreadId, &AgentPickerThreadEntry)> {
        self.ordered_threads()
            .into_iter()
            .filter(|(thread_id, entry)| {
                Some(*thread_id) != primary_thread_id
                    && entry
                        .agent_path
                        .as_deref()
                        .is_some_and(|agent_path| !agent_path.trim().is_empty())
            })
            .collect()
    }

    /// Returns tracked thread ids in the same stable order used by the picker.
    pub(crate) fn tracked_thread_ids(&self) -> Vec<ThreadId> {
        self.ordered_threads()
            .into_iter()
            .map(|(thread_id, _)| thread_id)
            .collect()
    }

    /// Returns the adjacent thread id for keyboard navigation in stable spawn order.
    ///
    /// The caller must pass the thread whose transcript is actually being shown to the user, not
    /// just whichever thread bookkeeping most recently marked active. If the wrong current thread
    /// is supplied, next/previous navigation will jump in a way that feels nondeterministic even
    /// though the cache itself is correct.
    pub(crate) fn adjacent_thread_id(
        &self,
        current_displayed_thread_id: Option<ThreadId>,
        direction: AgentNavigationDirection,
    ) -> Option<ThreadId> {
        let ordered_threads = self.ordered_threads();
        if ordered_threads.len() < 2 {
            return None;
        }

        let current_thread_id = current_displayed_thread_id?;
        let current_idx = ordered_threads
            .iter()
            .position(|(thread_id, _)| *thread_id == current_thread_id)?;
        let next_idx = match direction {
            AgentNavigationDirection::Next => (current_idx + 1) % ordered_threads.len(),
            AgentNavigationDirection::Previous => {
                if current_idx == 0 {
                    ordered_threads.len() - 1
                } else {
                    current_idx - 1
                }
            }
        };
        Some(ordered_threads[next_idx].0)
    }

    /// Derives the contextual footer label for the currently displayed thread.
    ///
    /// This intentionally returns `None` until there is more than one tracked thread so
    /// single-thread sessions do not waste footer space restating the obvious. When metadata for
    /// the displayed thread is missing, the label falls back to the same generic naming rules used
    /// by the picker.
    pub(crate) fn active_agent_label(
        &self,
        current_displayed_thread_id: Option<ThreadId>,
        primary_thread_id: Option<ThreadId>,
    ) -> Option<String> {
        if self.threads.len() <= 1 {
            return None;
        }

        let thread_id = current_displayed_thread_id?;
        let label = self.display_name(thread_id, primary_thread_id);
        let has_path_label = primary_thread_id != Some(thread_id)
            && self.threads.get(&thread_id).is_some_and(|entry| {
                entry.agent_nickname.is_none()
                    && entry.agent_role.is_none()
                    && entry
                        .agent_path
                        .as_deref()
                        .is_some_and(|agent_path| !agent_path.trim().is_empty())
            });
        Some(if has_path_label {
            format!("`{label}`")
        } else {
            label
        })
    }

    /// Builds the `/subagents` picker subtitle from the same canonical bindings used by key handling.
    ///
    /// Keeping this text derived from the actual shortcut helpers prevents the picker copy from
    /// drifting if the bindings ever change on one platform.
    pub(crate) fn picker_subtitle() -> String {
        let previous: Span<'static> = previous_agent_shortcut().into();
        let next: Span<'static> = next_agent_shortcut().into();
        format!(
            "Select an agent to watch. {} previous, {} next.",
            previous.content, next.content
        )
    }
}

#[cfg(test)]
#[path = "agent_navigation_tests.rs"]
mod tests;
