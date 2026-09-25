use super::initial_agent_response_observation_state;
use crate::session::tests::make_session_and_context_with_rx;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_history::RolloutItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnStartedEvent;
use pretty_assertions::assert_eq;
use std::sync::Arc;

#[tokio::test]
async fn recovered_active_turn_is_not_live_send_authority() {
    let (session, _turn, _events) = make_session_and_context_with_rx().await;
    let history = InitialHistory::Resumed(ResumedHistory {
        conversation_id: session.thread_id,
        history: Arc::new(vec![RolloutItem::EventMsg(EventMsg::TurnStarted(
            TurnStartedEvent {
                turn_id: "recovered-turn".into(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
                agent_queue: None,
            },
        ))]),
        rollout_path: None,
    });
    *session
        .response_observation_state
        .lock()
        .expect("response state") = initial_agent_response_observation_state(&history);

    let (snapshot, _subscription) = session.subscribe_agent_responses();
    assert_eq!(
        (
            snapshot.active_turn_id,
            session.active_agent_response_turn_id(),
        ),
        (Some("recovered-turn".into()), None),
    );

    assert!(session.begin_agent_response_turn("live-turn"));
    assert_eq!(
        session.active_agent_response_turn_id(),
        Some("live-turn".into()),
    );
}

#[tokio::test]
async fn late_terminal_cannot_clear_the_current_live_turn() {
    let (session, _turn, _events) = make_session_and_context_with_rx().await;
    assert!(session.begin_agent_response_turn("old-turn"));
    assert!(session.begin_agent_response_turn("current-turn"));

    session.publish_agent_response_terminal("old-turn".into(), AgentStatus::Completed(None));
    assert_eq!(
        session.active_agent_response_turn_id(),
        Some("current-turn".into()),
    );

    session.publish_agent_response_terminal("current-turn".into(), AgentStatus::Completed(None));
    assert_eq!(session.active_agent_response_turn_id(), None);
}
