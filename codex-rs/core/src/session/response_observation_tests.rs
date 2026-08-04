use super::AgentResponseEvent;
use super::agent_response_event;
use super::agent_response_events_from_rollout;
use super::initial_agent_response_observation_state;
use crate::session::TerminalStatusEvent;
use crate::session::tests::attach_thread_persistence;
use crate::session::tests::make_session_and_context_with_rx;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentMessageContentDeltaEvent;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ResumedHistory;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

#[test]
fn complete_commentary_item_becomes_observable_response() {
    let event = EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: codex_protocol::ThreadId::new(),
        turn_id: "turn-1".to_string(),
        item: TurnItem::AgentMessage(AgentMessageItem {
            id: "message-1".to_string(),
            content: vec![AgentMessageContent::Text {
                text: "I understand the revised scope.".to_string(),
            }],
            phase: Some(MessagePhase::Commentary),
            memory_citation: None,
            sub_agent_completion: None,
        }),
        started_at_ms: Some(0),
        completed_at_ms: 1,
    });

    assert_eq!(
        agent_response_event(&event, /*sequence*/ 7),
        Some(AgentResponseEvent::Commentary {
            turn_id: "turn-1".to_string(),
            item_id: "message-1".to_string(),
            text: "I understand the revised scope.".to_string(),
            sequence: 7,
        })
    );
}

#[test]
fn canonical_commentary_recovery_matches_legacy_and_paginated_representations() {
    let thread_id = ThreadId::new();
    let turn_started = RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: "turn-1".to_string(),
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
    }));
    let mut response_item = ResponseItem::Message {
        id: Some(ResponseItemId::new("msg")),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: "Recovered acknowledgement.".to_string(),
        }],
        phase: Some(MessagePhase::Commentary),
        internal_chat_message_metadata_passthrough: None,
    };
    response_item.set_turn_id_if_missing("turn-1");
    let item_id = response_item
        .id()
        .expect("assigned response item id")
        .to_string();
    let completed = RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id,
        turn_id: "turn-1".to_string(),
        item: TurnItem::AgentMessage(AgentMessageItem {
            id: item_id.clone(),
            content: vec![AgentMessageContent::Text {
                text: "Recovered acknowledgement.".to_string(),
            }],
            phase: Some(MessagePhase::Commentary),
            memory_citation: None,
            sub_agent_completion: None,
        }),
        started_at_ms: Some(0),
        completed_at_ms: 1,
    }));
    let expected = vec![
        AgentResponseEvent::TurnStarted {
            turn_id: "turn-1".to_string(),
            sequence: 0,
        },
        AgentResponseEvent::Commentary {
            turn_id: "turn-1".to_string(),
            item_id: item_id.clone(),
            text: "Recovered acknowledgement.".to_string(),
            sequence: 1,
        },
    ];
    let legacy_items = vec![
        turn_started.clone(),
        RolloutItem::ResponseItem(response_item.clone()),
    ];
    let paginated_items = vec![
        turn_started,
        RolloutItem::ResponseItem(response_item),
        completed,
    ];

    assert_eq!(agent_response_events_from_rollout(&legacy_items), expected);
    assert_eq!(
        agent_response_events_from_rollout(&paginated_items),
        expected
    );
    let state =
        initial_agent_response_observation_state(&InitialHistory::Resumed(ResumedHistory {
            conversation_id: thread_id,
            history: Arc::new(paginated_items),
            rollout_path: None,
        }));
    assert_eq!(
        (state.active_turn_id, state.last_commentary_item_id),
        (Some("turn-1".to_string()), Some(item_id))
    );
}

#[test]
fn final_answer_item_is_not_observed_as_commentary() {
    let event = EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: codex_protocol::ThreadId::new(),
        turn_id: "turn-1".to_string(),
        item: TurnItem::AgentMessage(AgentMessageItem {
            id: "message-1".to_string(),
            content: vec![AgentMessageContent::Text {
                text: "Done.".to_string(),
            }],
            phase: Some(MessagePhase::FinalAnswer),
            memory_citation: None,
            sub_agent_completion: None,
        }),
        started_at_ms: Some(0),
        completed_at_ms: 1,
    });

    assert_eq!(agent_response_event(&event, /*sequence*/ 0), None);
}

#[test]
fn streaming_agent_message_fragments_are_not_observed_as_commentary() {
    let event = EventMsg::AgentMessageContentDelta(AgentMessageContentDeltaEvent {
        thread_id: ThreadId::new().to_string(),
        turn_id: "turn-1".to_string(),
        item_id: "message-1".to_string(),
        delta: "partial acknowledgement".to_string(),
    });

    assert_eq!(agent_response_event(&event, /*sequence*/ 0), None);
}

#[test]
fn interrupted_turn_becomes_a_non_delivering_turn_boundary() {
    let event = EventMsg::TurnAborted(TurnAbortedEvent {
        turn_id: Some("turn-1".to_string()),
        reason: TurnAbortReason::Interrupted,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    });

    assert_eq!(
        agent_response_event(&event, /*sequence*/ 4),
        Some(AgentResponseEvent::TurnAborted {
            turn_id: "turn-1".to_string(),
        })
    );
}

#[test]
fn response_snapshot_reconstructs_the_exact_last_terminal_turn() {
    let observer_thread_id = ThreadId::new();
    let history = InitialHistory::Resumed(ResumedHistory {
        conversation_id: observer_thread_id,
        history: Arc::new(vec![
            RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "completed-turn".to_string(),
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
            })),
            RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "completed-turn".to_string(),
                last_agent_message: Some("done".to_string()),
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            })),
        ]),
        rollout_path: None,
    });

    let state = initial_agent_response_observation_state(&history);

    assert_eq!(state.active_turn_id, None);
    assert_eq!(state.next_event_sequence, 2);
    assert_eq!(
        state.last_terminal,
        Some((
            "completed-turn".to_string(),
            AgentStatus::Completed(Some("done".to_string()))
        ))
    );
}

#[test]
fn response_snapshot_keeps_newer_completed_turn_after_delayed_historical_terminal() {
    let thread_id = ThreadId::new();
    let history = InitialHistory::Resumed(ResumedHistory {
        conversation_id: thread_id,
        history: Arc::new(vec![
            RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "turn-a".to_string(),
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
            })),
            RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "turn-b".to_string(),
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
            })),
            RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-b".to_string(),
                last_agent_message: Some("newer result".to_string()),
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            })),
            RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-a".to_string(),
                last_agent_message: Some("delayed historical result".to_string()),
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            })),
        ]),
        rollout_path: None,
    });

    let state = initial_agent_response_observation_state(&history);

    assert_eq!(
        (
            state.active_turn_id,
            state.latest_admitted_turn_id,
            state.last_terminal,
        ),
        (
            None,
            Some("turn-b".to_string()),
            Some((
                "turn-b".to_string(),
                AgentStatus::Completed(Some("newer result".to_string())),
            )),
        )
    );
}

#[tokio::test]
async fn delayed_historical_terminal_does_not_replace_live_newer_status() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    for msg in [
        EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-a".to_string(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        }),
        EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-b".to_string(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        }),
        EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-b".to_string(),
            last_agent_message: Some("newer result".to_string()),
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        }),
        EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-a".to_string(),
            last_agent_message: Some("delayed historical result".to_string()),
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        }),
    ] {
        assert!(session.publish_agent_response_event_and_status(&Event {
            id: String::new(),
            msg,
        }));
    }

    let (snapshot, _subscription) = session.subscribe_terminal_status();
    assert_eq!(
        snapshot,
        TerminalStatusEvent {
            turn_id: Some("turn-b".to_string()),
            status: AgentStatus::Completed(Some("newer result".to_string())),
        }
    );
}

#[test]
fn response_snapshot_excludes_exactly_rolled_back_turns_for_resume_and_fork() {
    let thread_id = ThreadId::new();
    let rolled_back_turn_id = "rolled-back-turn";
    let rolled_back_item_id = "rolled-back-commentary";
    let items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "retained-turn".to_string(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        })),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "retained-turn".to_string(),
            last_agent_message: Some("retained result".to_string()),
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: rolled_back_turn_id.to_string(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        })),
        RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id,
            turn_id: rolled_back_turn_id.to_string(),
            item: TurnItem::AgentMessage(AgentMessageItem {
                id: rolled_back_item_id.to_string(),
                content: vec![AgentMessageContent::Text {
                    text: "rolled back acknowledgement".to_string(),
                }],
                phase: Some(MessagePhase::Commentary),
                memory_citation: None,
                sub_agent_completion: None,
            }),
            started_at_ms: Some(0),
            completed_at_ms: 1,
        })),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: rolled_back_turn_id.to_string(),
            last_agent_message: Some("rolled back result".to_string()),
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            rollback_start_index: Some(2),
            ..Default::default()
        })),
    ];
    let histories = [
        InitialHistory::Forked(items.clone()),
        InitialHistory::Resumed(ResumedHistory {
            conversation_id: thread_id,
            history: Arc::new(items),
            rollout_path: None,
        }),
    ];

    for history in histories {
        let state = initial_agent_response_observation_state(&history);
        assert_eq!(
            (
                state.active_turn_id,
                state.last_terminal,
                state.next_event_sequence,
                state.last_commentary_item_id,
            ),
            (
                None,
                Some((
                    "retained-turn".to_string(),
                    AgentStatus::Completed(Some("retained result".to_string())),
                )),
                2,
                None,
            )
        );
    }
}

#[tokio::test]
async fn response_subscription_is_serialized_with_terminal_publication() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    let terminal_guard = session
        .terminal_publication_lock
        .lock()
        .expect("terminal publication lock");
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(/*bound*/ 1);
    let (subscribed_tx, subscribed_rx) = std::sync::mpsc::sync_channel(/*bound*/ 1);
    let subscribing_session = Arc::clone(&session);
    let subscriber = std::thread::spawn(move || {
        started_tx.send(()).expect("subscription start signal");
        let (snapshot, _subscription) =
            subscribing_session.subscribe_agent_responses_observing_terminal(|_, _| {});
        subscribed_tx
            .send(snapshot.status)
            .expect("subscription result");
    });
    started_rx.recv().expect("subscription should start");
    assert_eq!(
        subscribed_rx.recv_timeout(Duration::from_millis(50)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    );

    let terminal_status = AgentStatus::Completed(Some("done".to_string()));
    session.agent_status.send_replace(terminal_status.clone());
    drop(terminal_guard);

    assert_eq!(
        subscribed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("subscription should cross the released boundary"),
        terminal_status
    );
    subscriber.join().expect("subscriber should finish");
}

#[tokio::test]
async fn live_terminal_remains_observable_after_child_rollout_persistence_fails() {
    let (mut session, _turn_context, _events) = make_session_and_context_with_rx().await;
    attach_thread_persistence(Arc::get_mut(&mut session).expect("unique session")).await;
    session
        .live_thread()
        .expect("attached live thread")
        .shutdown()
        .await
        .expect("shut down child rollout writer");
    let (_snapshot, mut response_rx) = session.subscribe_agent_responses();

    session
        .send_event_raw(Event {
            id: "turn-1".to_string(),
            msg: EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: session.thread_id,
                turn_id: "turn-1".to_string(),
                item: TurnItem::AgentMessage(AgentMessageItem {
                    id: "commentary-1".to_string(),
                    content: vec![AgentMessageContent::Text {
                        text: "This source item was not persisted.".to_string(),
                    }],
                    phase: Some(MessagePhase::Commentary),
                    memory_citation: None,
                    sub_agent_completion: None,
                }),
                started_at_ms: Some(0),
                completed_at_ms: 1,
            }),
        })
        .await;
    session
        .send_event_raw(Event {
            id: "turn-1".to_string(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: Some("live terminal result".to_string()),
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        })
        .await;

    assert_eq!(
        timeout(Duration::from_secs(1), response_rx.recv())
            .await
            .expect("live terminal response should not remain blocked"),
        Some(AgentResponseEvent::Terminal {
            turn_id: "turn-1".to_string(),
            status: AgentStatus::Completed(Some("live terminal result".to_string())),
        })
    );
}
