//! Agent-turn lifecycle state for `ChatWidget`.

use std::collections::HashSet;
use std::collections::VecDeque;
use std::time::Instant;

use codex_utils_sleep_inhibitor::SleepInhibitor;

use super::sleep::ActiveSleep;

const MAX_RECENTLY_HANDLED_SLEEP_ITEMS: usize = 32;

#[derive(Debug)]
pub(super) struct TurnLifecycleState {
    pub(super) sleep_inhibitor: SleepInhibitor,
    /// Tracks whether codex-core currently considers an agent turn to be in progress.
    pub(super) agent_turn_running: bool,
    turn_started_at: Option<Instant>,
    pub(super) last_turn_id: Option<String>,
    pub(super) budget_limited_turn_ids: HashSet<String>,
    pub(super) goal_status_active_turn_started_at: Option<Instant>,
    pub(super) active_sleep: Option<ActiveSleep>,
    /// Recent completed sleep identities prevent late duplicate notifications from rendering twice.
    recently_handled_sleep_item_ids: VecDeque<(String, String)>,
}

impl TurnLifecycleState {
    pub(super) fn new(prevent_idle_sleep: bool) -> Self {
        Self {
            sleep_inhibitor: SleepInhibitor::new(prevent_idle_sleep),
            agent_turn_running: false,
            turn_started_at: None,
            last_turn_id: None,
            budget_limited_turn_ids: HashSet::new(),
            goal_status_active_turn_started_at: None,
            active_sleep: None,
            recently_handled_sleep_item_ids: VecDeque::new(),
        }
    }

    pub(super) fn start(&mut self, now: Instant) {
        self.agent_turn_running = true;
        self.turn_started_at = Some(now);
        self.goal_status_active_turn_started_at = Some(now);
        self.sleep_inhibitor.set_turn_running(/*turn_running*/ true);
    }

    pub(super) fn finish(&mut self) {
        self.agent_turn_running = false;
        self.turn_started_at = None;
        self.goal_status_active_turn_started_at = None;
        self.sleep_inhibitor
            .set_turn_running(/*turn_running*/ false);
    }

    pub(super) fn restore_running_since(&mut self, started_at: Instant) {
        self.agent_turn_running = true;
        self.turn_started_at = Some(started_at);
        self.goal_status_active_turn_started_at = Some(started_at);
        self.sleep_inhibitor.set_turn_running(/*turn_running*/ true);
    }

    pub(super) fn started_at(&self) -> Option<Instant> {
        self.turn_started_at
    }

    pub(super) fn elapsed_seconds(&self, now: Instant) -> Option<u64> {
        self.turn_started_at
            .map(|started_at| now.saturating_duration_since(started_at).as_secs())
    }

    pub(super) fn reset_thread(&mut self) {
        self.finish();
        self.last_turn_id = None;
        self.budget_limited_turn_ids.clear();
        self.recently_handled_sleep_item_ids.clear();
        self.active_sleep = None;
    }

    pub(super) fn set_prevent_idle_sleep(&mut self, enabled: bool) {
        self.sleep_inhibitor = SleepInhibitor::new(enabled);
        self.sleep_inhibitor
            .set_turn_running(self.agent_turn_running);
    }

    pub(super) fn mark_budget_limited(&mut self, turn_id: String) {
        self.budget_limited_turn_ids.insert(turn_id);
    }

    pub(super) fn take_budget_limited(&mut self, turn_id: &str) -> bool {
        self.budget_limited_turn_ids.remove(turn_id)
    }

    pub(super) fn has_handled_sleep_item(&self, turn_id: &str, item_id: &str) -> bool {
        self.recently_handled_sleep_item_ids
            .iter()
            .any(|(handled_turn_id, handled_item_id)| {
                handled_turn_id == turn_id && handled_item_id == item_id
            })
    }

    /// Remembers a recent sleep item identity, returning whether it was new.
    pub(super) fn remember_sleep_item(&mut self, turn_id: &str, item_id: &str) -> bool {
        if self.has_handled_sleep_item(turn_id, item_id) {
            return false;
        }
        self.recently_handled_sleep_item_ids
            .push_back((turn_id.to_string(), item_id.to_string()));
        if self.recently_handled_sleep_item_ids.len() > MAX_RECENTLY_HANDLED_SLEEP_ITEMS {
            let _ = self.recently_handled_sleep_item_ids.pop_front();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_and_finish_update_running_state() {
        let mut state = TurnLifecycleState::new(/*prevent_idle_sleep*/ false);
        let started_at = Instant::now();

        state.start(started_at);
        assert!(state.agent_turn_running);
        assert!(state.goal_status_active_turn_started_at.is_some());
        assert!(state.sleep_inhibitor.is_turn_running());
        assert_eq!(
            state.elapsed_seconds(started_at + std::time::Duration::from_secs(65)),
            Some(65)
        );

        state.finish();
        assert!(!state.agent_turn_running);
        assert!(state.goal_status_active_turn_started_at.is_none());
        assert!(!state.sleep_inhibitor.is_turn_running());
        assert_eq!(state.elapsed_seconds(Instant::now()), None);
    }

    #[test]
    fn restore_running_since_preserves_turn_origin() {
        let mut state = TurnLifecycleState::new(/*prevent_idle_sleep*/ false);
        let started_at = Instant::now();

        state.restore_running_since(started_at);

        assert_eq!(
            state.elapsed_seconds(started_at + std::time::Duration::from_secs(65)),
            Some(65)
        );
    }

    #[test]
    fn budget_limited_turn_ids_are_consumed() {
        let mut state = TurnLifecycleState::new(/*prevent_idle_sleep*/ false);

        state.mark_budget_limited("turn-1".to_string());

        assert!(state.take_budget_limited("turn-1"));
        assert!(!state.take_budget_limited("turn-1"));
    }
}
