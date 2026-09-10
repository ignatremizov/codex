use super::*;
use pretty_assertions::assert_eq;

fn state_with_threads() -> (AgentNavigationState, [ThreadId; 4]) {
    let ids = std::array::from_fn(|_| ThreadId::new());
    let mut state = AgentNavigationState::default();
    for id in ids {
        state.upsert(
            id, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
        );
        if id != ids[0] {
            state.set_parent_thread_id(id, Some(ids[0]));
        }
    }
    (state, ids)
}

#[test]
fn cycle_skips_closed_and_unavailable_without_removing_picker_rows() {
    let (mut state, [root, closed, unavailable, idle]) = state_with_threads();
    state.mark_closed(closed);
    assert_eq!(
        [
            state.adjacent_available_thread_id(
                root,
                Some(root),
                AgentNavigationDirection::Next,
                |id| id != unavailable
            ),
            state.adjacent_available_thread_id(
                root,
                Some(root),
                AgentNavigationDirection::Previous,
                |id| id != unavailable
            ),
            state.adjacent_available_thread_id(
                root,
                Some(closed),
                AgentNavigationDirection::Next,
                |id| id != unavailable
            ),
            state.adjacent_available_thread_id(
                root,
                Some(idle),
                AgentNavigationDirection::Next,
                |id| id != unavailable
            ),
        ],
        [Some(idle), Some(idle), Some(idle), Some(root)]
    );
    assert_eq!(
        state.tracked_thread_ids(),
        vec![root, closed, unavailable, idle]
    );
}

#[test]
fn cycle_accepts_running_and_idle_but_excludes_foreign_and_transferred_members() {
    let (mut state, [root, running, foreign, transferred]) = state_with_threads();
    state.mark_running(running);
    state.set_parent_thread_id(foreign, Some(ThreadId::new()));
    state.upsert_alias(
        transferred,
        /*agent_ref*/ 4,
        /*nickname*/ None,
        AgentAliasState::Transferred,
    );
    assert_eq!(
        [
            state.adjacent_available_thread_id(
                root,
                Some(root),
                AgentNavigationDirection::Next,
                |_| true
            ),
            state.adjacent_available_thread_id(
                root,
                Some(running),
                AgentNavigationDirection::Next,
                |_| true
            ),
        ],
        [Some(running), Some(root)]
    );
}

#[test]
fn cycle_uses_current_root_aliases_for_adopted_members_and_bounds_malformed_ancestry() {
    let (mut state, [root, adopted, first, second]) = state_with_threads();
    state.upsert_alias(
        root,
        /*agent_ref*/ 1,
        Some("Main".to_string()),
        AgentAliasState::Active,
    );
    state.upsert_alias(
        adopted,
        /*agent_ref*/ 2,
        Some("worker".to_string()),
        AgentAliasState::Active,
    );
    state.set_parent_thread_id(adopted, Some(ThreadId::new()));
    state.set_parent_thread_id(first, Some(second));
    state.set_parent_thread_id(second, Some(first));
    assert_eq!(
        state.adjacent_available_thread_id(
            root,
            Some(root),
            AgentNavigationDirection::Next,
            |_| true
        ),
        Some(adopted)
    );
    let other_root = ThreadId::new();
    assert_eq!(
        state.adjacent_available_thread_id(
            other_root,
            Some(root),
            AgentNavigationDirection::Next,
            |_| true
        ),
        None
    );
}

#[test]
fn cycle_returns_none_when_only_current_is_available_or_anchor_is_unknown() {
    let (state, [root, _, _, _]) = state_with_threads();
    assert_eq!(
        [
            state.adjacent_available_thread_id(
                root,
                Some(root),
                AgentNavigationDirection::Next,
                |id| id == root
            ),
            state.adjacent_available_thread_id(
                root,
                Some(ThreadId::new()),
                AgentNavigationDirection::Previous,
                |_| true
            ),
            state.adjacent_available_thread_id(
                root,
                /*current*/ None,
                AgentNavigationDirection::Next,
                |_| true
            ),
        ],
        [None, None, None]
    );
}
