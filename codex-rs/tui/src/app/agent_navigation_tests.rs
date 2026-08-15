use pretty_assertions::assert_eq;

use super::*;
use crate::app::agent_observation_display::AgentFinalResponseDisplay;
use crate::app::agent_observation_display::AgentResponseObservationDisplay;

fn populated_state() -> (AgentNavigationState, ThreadId, ThreadId, ThreadId) {
    let mut state = AgentNavigationState::default();
    let main_thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000101").expect("valid thread");
    let first_agent_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000102").expect("valid thread");
    let second_agent_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000103").expect("valid thread");

    state.upsert(
        main_thread_id,
        /*agent_nickname*/ None,
        /*agent_role*/ None,
        /*is_closed*/ false,
    );
    state.upsert(
        first_agent_id,
        Some("Robie".to_string()),
        Some("explorer".to_string()),
        /*is_closed*/ false,
    );
    state.upsert(
        second_agent_id,
        Some("Bob".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ false,
    );

    (state, main_thread_id, first_agent_id, second_agent_id)
}

fn ordered_thread_ids(state: &AgentNavigationState) -> Vec<ThreadId> {
    state
        .ordered_threads()
        .into_iter()
        .map(|(thread_id, _)| thread_id)
        .collect()
}

#[test]
fn upsert_preserves_first_seen_order() {
    let (mut state, main_thread_id, first_agent_id, second_agent_id) = populated_state();

    state.upsert(
        first_agent_id,
        Some("Robie".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ true,
    );

    assert_eq!(
        ordered_thread_ids(&state),
        vec![main_thread_id, first_agent_id, second_agent_id]
    );
}

#[test]
fn parent_thread_id_tracks_immediate_parent_until_removal() {
    let (mut state, main_thread_id, first_agent_id, second_agent_id) = populated_state();

    state.set_parent_thread_id(first_agent_id, Some(main_thread_id));
    state.set_parent_thread_id(second_agent_id, Some(first_agent_id));

    assert_eq!(
        state.parent_thread_id(second_agent_id),
        Some(first_agent_id)
    );
    assert_eq!(state.depth(main_thread_id), 0);
    assert_eq!(state.depth(first_agent_id), 1);
    assert_eq!(state.depth(second_agent_id), 2);
    state.remove(first_agent_id);
    assert_eq!(state.parent_thread_id(first_agent_id), None);
    assert_eq!(
        state.parent_thread_id(second_agent_id),
        Some(first_agent_id)
    );
    state.clear();
    assert_eq!(state.parent_thread_id(second_agent_id), None);
}

#[test]
fn wake_subscriptions_are_observer_relative_and_end_with_the_target_turn() {
    let (mut state, main_thread_id, first_agent_id, second_agent_id) = populated_state();

    state.note_response_observation(
        main_thread_id,
        first_agent_id,
        AgentResponseObservationBinding::Bound,
        Some(AgentResponseHandling::Wake),
    );
    state.note_response_observation(
        first_agent_id,
        second_agent_id,
        AgentResponseObservationBinding::Bound,
        Some(AgentResponseHandling::Wake),
    );

    assert!(state.has_wake_subscription(main_thread_id, first_agent_id));
    assert!(state.has_wake_subscription(first_agent_id, second_agent_id));
    assert!(!state.has_wake_subscription(main_thread_id, second_agent_id));

    state.mark_stopped(first_agent_id);

    assert!(!state.has_wake_subscription(main_thread_id, first_agent_id));
    assert!(
        state.has_wake_subscription(first_agent_id, second_agent_id),
        "an observer's own turn ending must not cancel its target subscription"
    );

    state.mark_closed(first_agent_id);

    assert!(!state.has_wake_subscription(first_agent_id, second_agent_id));

    state.note_response_observation(
        main_thread_id,
        second_agent_id,
        AgentResponseObservationBinding::NextTurn,
        Some(AgentResponseHandling::Wake),
    );
    state.mark_stopped(second_agent_id);
    assert!(state.has_wake_subscription(main_thread_id, second_agent_id));
    state.mark_running(second_agent_id);
    state.mark_stopped(second_agent_id);
    assert!(!state.has_wake_subscription(main_thread_id, second_agent_id));
}

#[test]
fn user_response_observation_tracks_commentary_final_mode_and_turn_binding() {
    let (mut state, main_thread_id, first_agent_id, _second_agent_id) = populated_state();

    state.note_response_observation(
        main_thread_id,
        first_agent_id,
        AgentResponseObservationBinding::NextTurn,
        Some(AgentResponseHandling::CommentaryPresentation),
    );
    assert_eq!(
        state.response_observation(main_thread_id, first_agent_id),
        Some(AgentResponseObservationDisplay {
            binding: AgentResponseObservationBinding::NextTurn,
            commentary: true,
            target_messages: false,
            queue_delivery: false,
            final_response: AgentFinalResponseDisplay::Presentation,
        })
    );

    state.mark_running(first_agent_id);
    state.replace_user_final_response_observation(
        main_thread_id,
        first_agent_id,
        AgentResponseObservationBinding::Bound,
        AgentFinalResponseHandling::Wake,
    );
    assert_eq!(
        state.response_observation(main_thread_id, first_agent_id),
        Some(AgentResponseObservationDisplay {
            binding: AgentResponseObservationBinding::Bound,
            commentary: true,
            target_messages: false,
            queue_delivery: false,
            final_response: AgentFinalResponseDisplay::Wake,
        })
    );

    state.mark_stopped(first_agent_id);
    assert_eq!(
        state.response_observation(main_thread_id, first_agent_id),
        None
    );
}

#[test]
fn reserved_prompt_response_ends_when_target_starts_or_closes() {
    let (mut state, main_thread_id, first_agent_id, second_agent_id) = populated_state();

    state.note_response_observation(
        main_thread_id,
        first_agent_id,
        AgentResponseObservationBinding::NextTurn,
        Some(AgentResponseHandling::CommentaryWake),
    );
    state.reserve_prompt_response(main_thread_id, first_agent_id);
    state.reserve_prompt_response(first_agent_id, second_agent_id);
    assert_eq!(
        state.reserved_prompt_source(first_agent_id),
        Some(main_thread_id)
    );
    assert_eq!(
        state.reserved_prompt_source(second_agent_id),
        Some(first_agent_id)
    );

    state.mark_running(first_agent_id);
    assert_eq!(state.reserved_prompt_source(first_agent_id), None);
    assert_eq!(
        state.response_observation(main_thread_id, first_agent_id),
        Some(AgentResponseObservationDisplay {
            binding: AgentResponseObservationBinding::Bound,
            commentary: true,
            target_messages: false,
            queue_delivery: false,
            final_response: AgentFinalResponseDisplay::Wake,
        })
    );
    assert_eq!(
        state.reserved_prompt_source(second_agent_id),
        Some(first_agent_id)
    );

    state.mark_closed(second_agent_id);
    assert_eq!(state.reserved_prompt_source(second_agent_id), None);
}

#[test]
fn upsert_preserves_known_identity_when_update_omits_metadata() {
    let mut state = AgentNavigationState::default();
    let thread_id = ThreadId::new();
    state.upsert(
        thread_id,
        Some("Herschel".to_string()),
        Some("default".to_string()),
        /*is_closed*/ true,
    );

    state.upsert(
        thread_id, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
    );

    assert_eq!(
        state.get(&thread_id),
        Some(&AgentPickerThreadEntry {
            agent_nickname: Some("Herschel".to_string()),
            agent_role: Some("default".to_string()),
            agent_path: None,
            is_running: false,
            is_closed: false,
        })
    );
}

#[test]
fn durable_aliases_resolve_refs_and_nicknames_but_not_transferred_reservations() {
    let (mut state, main_thread_id, first_agent_id, second_agent_id) = populated_state();
    state.replace_aliases(vec![
        AgentAlias {
            thread_id: main_thread_id.to_string(),
            agent_ref: "1".to_string(),
            nickname: Some(MAIN_AGENT_NICKNAME.to_string()),
            state: AgentAliasState::Active,
        },
        AgentAlias {
            thread_id: first_agent_id.to_string(),
            agent_ref: "2".to_string(),
            nickname: Some("Robie".to_string()),
            state: AgentAliasState::Closed,
        },
        AgentAlias {
            thread_id: second_agent_id.to_string(),
            agent_ref: "3".to_string(),
            nickname: Some("Bob".to_string()),
            state: AgentAliasState::Transferred,
        },
    ]);

    assert_eq!(state.root_thread_id(), Some(main_thread_id));
    assert_eq!(state.thread_id_for_ref(1), Some(main_thread_id));
    assert_eq!(state.thread_id_for_ref(2), Some(first_agent_id));
    assert_eq!(state.thread_id_for_nickname("mAiN"), Some(main_thread_id));
    assert_eq!(state.thread_id_for_nickname("Robie"), Some(first_agent_id));
    assert_eq!(state.thread_id_for_nickname("robie"), None);
    assert_eq!(
        state.control_selector(main_thread_id).as_deref(),
        Some(MAIN_AGENT_NICKNAME)
    );
    assert_eq!(state.control_selector(first_agent_id).as_deref(), Some("2"));
    assert_eq!(state.thread_id_for_ref(3), None);
    assert_eq!(state.thread_id_for_nickname("Bob"), None);
    state.clear();
    assert_eq!(state.root_thread_id(), None);
    assert_eq!(state.thread_id_for_ref(2), None);
}

#[test]
fn durable_alias_without_nickname_clears_stale_thread_metadata() {
    let mut state = AgentNavigationState::default();
    let thread_id = ThreadId::new();
    state.upsert(
        thread_id,
        Some("Colliding historical name".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ false,
    );

    state.replace_aliases(vec![AgentAlias {
        thread_id: thread_id.to_string(),
        agent_ref: "2".to_string(),
        nickname: None,
        state: AgentAliasState::Active,
    }]);

    assert_eq!(
        state.get(&thread_id),
        Some(&AgentPickerThreadEntry {
            agent_nickname: None,
            agent_role: Some("worker".to_string()),
            agent_path: None,
            is_running: false,
            is_closed: false,
        })
    );
    assert_eq!(
        state.authoritative_nickname(thread_id, Some("Stale again".to_string())),
        None
    );
}

#[test]
fn committed_spawn_alias_is_immediately_resolvable_before_refresh() {
    let mut state = AgentNavigationState::default();
    let thread_id = ThreadId::new();
    state.upsert(
        thread_id,
        /*agent_nickname*/ None,
        Some("reviewer".to_string()),
        /*is_closed*/ false,
    );

    state.upsert_alias(
        thread_id,
        /*agent_ref*/ 2,
        Some("Parfit".to_string()),
        codex_app_server_protocol::AgentAliasState::Active,
    );

    assert_eq!(state.thread_id_for_ref(2), Some(thread_id));
    assert_eq!(state.thread_id_for_nickname("Parfit"), Some(thread_id));
    assert_eq!(
        state.get(&thread_id).map(|entry| (
            entry.agent_nickname.clone(),
            entry.agent_role.clone(),
            entry.is_closed,
        )),
        Some((
            Some("Parfit".to_string()),
            Some("reviewer".to_string()),
            false,
        ))
    );
}

#[test]
fn durable_identity_takes_precedence_over_internal_agent_path() {
    let mut state = AgentNavigationState::default();
    let thread_id = ThreadId::new();
    state.upsert(
        thread_id,
        Some("Parfit".to_string()),
        Some("reviewer".to_string()),
        /*is_closed*/ false,
    );
    state.set_agent_path(thread_id, Some("/root/reviewer".to_string()));

    assert_eq!(
        state.display_name(thread_id, /*primary_thread_id*/ None),
        "Parfit [reviewer]"
    );
    state.upsert(
        ThreadId::new(),
        /*agent_nickname*/ None,
        /*agent_role*/ None,
        /*is_closed*/ false,
    );
    assert_eq!(
        state.active_agent_label(Some(thread_id), /*primary_thread_id*/ None),
        Some("Parfit [reviewer]".to_string())
    );
}

#[test]
fn durable_refs_restore_cold_picker_order() {
    let root = ThreadId::new();
    let first = ThreadId::new();
    let second = ThreadId::new();
    let mut state = AgentNavigationState::default();
    state.upsert(
        second, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
    );
    state.upsert(
        root, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
    );
    state.upsert(
        first, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ true,
    );
    state.replace_aliases(vec![
        AgentAlias {
            thread_id: second.to_string(),
            agent_ref: "3".to_string(),
            nickname: None,
            state: AgentAliasState::Active,
        },
        AgentAlias {
            thread_id: root.to_string(),
            agent_ref: "1".to_string(),
            nickname: Some("Main".to_string()),
            state: AgentAliasState::Active,
        },
        AgentAlias {
            thread_id: first.to_string(),
            agent_ref: "2".to_string(),
            nickname: None,
            state: AgentAliasState::Transferred,
        },
    ]);

    state.order_by_agent_ref();

    assert_eq!(ordered_thread_ids(&state), vec![root, first, second]);
}

#[test]
fn picker_refresh_rejects_responses_from_before_clear() {
    let mut state = AgentNavigationState::default();
    let thread_id = ThreadId::new();
    let stale_request = state
        .begin_picker_refresh(thread_id)
        .expect("first picker refresh");

    assert_eq!(state.begin_picker_refresh(thread_id), None);
    state.clear();
    let current_request = state
        .begin_picker_refresh(thread_id)
        .expect("refresh after session reset");

    assert!(!state.finish_picker_refresh(thread_id, stale_request));
    assert!(state.finish_picker_refresh(thread_id, current_request));
}

#[test]
fn adjacent_thread_id_wraps_in_spawn_order() {
    let (state, main_thread_id, first_agent_id, second_agent_id) = populated_state();

    assert_eq!(
        state.adjacent_thread_id(Some(second_agent_id), AgentNavigationDirection::Next),
        Some(main_thread_id)
    );
    assert_eq!(
        state.adjacent_thread_id(Some(second_agent_id), AgentNavigationDirection::Previous),
        Some(first_agent_id)
    );
    assert_eq!(
        state.adjacent_thread_id(Some(main_thread_id), AgentNavigationDirection::Previous),
        Some(second_agent_id)
    );
}

#[test]
fn picker_subtitle_mentions_shortcuts() {
    let previous: Span<'static> = previous_agent_shortcut().into();
    let next: Span<'static> = next_agent_shortcut().into();
    let subtitle = AgentNavigationState::picker_subtitle();

    assert!(subtitle.contains(previous.content.as_ref()));
    assert!(subtitle.contains(next.content.as_ref()));
}

#[test]
fn active_agent_label_tracks_current_thread() {
    let (state, main_thread_id, first_agent_id, _) = populated_state();

    assert_eq!(
        state.active_agent_label(Some(first_agent_id), Some(main_thread_id)),
        Some("Robie [explorer]".to_string())
    );
    assert_eq!(
        state.active_agent_label(Some(main_thread_id), Some(main_thread_id)),
        Some("Main [default]".to_string())
    );
}
