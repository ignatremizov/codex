use super::AgentResponseEvent;
use super::InputTurnAdmissionPolicy;
use super::InputTurnAdmissionResolution;
use super::agent_response_event;
use super::agent_response_events_from_rollout;
use super::initial_agent_response_observation_state;
use crate::session::TerminalStatusEvent;
use crate::session::tests::attach_thread_persistence;
use crate::session::tests::make_session_and_context_with_rx;
use codex_history::InitialHistory;
use codex_history::ResponseItemEnvelope;
use codex_history::ResumedHistory;
use codex_history::RolloutItem;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentMessageContentDeltaEvent;
use codex_protocol::protocol::AgentQueueResponseHandling;
use codex_protocol::protocol::AgentQueueTurnMetadata;
use codex_protocol::protocol::AgentResponseFinalDelivery;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::new_attributed_agent_message_response_item_id;
use futures::poll;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

fn queue_turn_metadata() -> AgentQueueTurnMetadata {
    AgentQueueTurnMetadata {
        queue_id: "00000000-0000-7000-8000-000000000001".to_string(),
        source_thread_id: ThreadId::new(),
        response_handling: Some(AgentQueueResponseHandling {
            commentary: false,
            final_delivery: AgentResponseFinalDelivery::Wake,
            target_messages: false,
        }),
    }
}

#[tokio::test]
async fn canceled_waiter_preserves_queue_admission_without_advertising_uncommitted_handling() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    let submission_id = "queued-submission".to_string();
    let queue_metadata = queue_turn_metadata();
    let mut admission = session.register_input_turn_admission(
        submission_id.clone(),
        InputTurnAdmissionPolicy::Queued,
        Some(queue_metadata.clone()),
    );

    admission.mark_submitted();
    drop(admission);

    assert_eq!(
        session.input_turn_admission_policy(&submission_id),
        InputTurnAdmissionPolicy::Queued
    );

    session.resolve_input_turn_admission(
        &submission_id,
        InputTurnAdmissionResolution {
            target_turn_id: "queued-turn".to_string(),
            minimum_event_sequence: 0,
            after_item_id: None,
        },
    );
    assert_eq!(
        session.input_turn_admission_policy(&submission_id),
        InputTurnAdmissionPolicy::AnyTurn
    );
    assert_eq!(
        session.await_agent_queue_turn_metadata("queued-turn").await,
        Some(AgentQueueTurnMetadata {
            response_handling: None,
            ..queue_metadata
        })
    );
}

#[tokio::test]
async fn queued_turn_startup_waits_for_source_commit() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    let submission_id = "queued-submission".to_string();
    let queue_metadata = queue_turn_metadata();
    let mut admission = session.register_input_turn_admission(
        submission_id.clone(),
        InputTurnAdmissionPolicy::Queued,
        Some(queue_metadata.clone()),
    );
    admission.mark_submitted();
    session.resolve_input_turn_admission(
        &submission_id,
        InputTurnAdmissionResolution {
            target_turn_id: "queued-turn".to_string(),
            minimum_event_sequence: 0,
            after_item_id: None,
        },
    );
    let outcome = admission
        .recv()
        .await
        .expect("admission should resolve")
        .expect("admission should succeed");
    let queue_start_permit = outcome
        .queue_start_permit
        .expect("queued admission should return its start permit");
    let mut metadata = Box::pin(session.await_agent_queue_turn_metadata("queued-turn"));
    assert!(poll!(metadata.as_mut()).is_pending());
    queue_start_permit.publish();
    assert_eq!(metadata.await, Some(queue_metadata));
    assert_eq!(
        session.await_agent_queue_turn_metadata("queued-turn").await,
        None
    );
}

#[tokio::test]
async fn delayed_queue_start_commit_survives_forced_abort_window() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    let submission_id = "queued-submission".to_string();
    let target_turn_id = turn_context.sub_id.clone();
    let queue_metadata = queue_turn_metadata();
    let mut admission = session.register_input_turn_admission(
        submission_id.clone(),
        InputTurnAdmissionPolicy::Queued,
        Some(queue_metadata.clone()),
    );
    admission.mark_submitted();
    session.resolve_input_turn_admission(
        &submission_id,
        InputTurnAdmissionResolution {
            target_turn_id: target_turn_id.clone(),
            minimum_event_sequence: 0,
            after_item_id: None,
        },
    );
    let outcome = admission
        .recv()
        .await
        .expect("admission should resolve")
        .expect("admission should succeed");
    let queue_start_permit = outcome
        .queue_start_permit
        .expect("queued admission should return its start permit");
    session
        .spawn_task(
            Arc::clone(&turn_context),
            vec![crate::session::TurnInput::UserInput {
                content: vec![codex_protocol::user_input::UserInput::Text {
                    text: "queued task".to_string(),
                    text_elements: Vec::new(),
                }],
                client_id: None,
            }],
            crate::tasks::RegularTask::new(),
        )
        .await;
    session
        .steer_input_with_response_observation_boundary_and_policy(
            crate::session::PromptTurnDraft {
                input: vec![codex_protocol::user_input::UserInput::Text {
                    text: "steered task".to_string(),
                    text_elements: Vec::new(),
                }],
                additional_context: Default::default(),
                client_user_message_id: None,
                responsesapi_client_metadata: None,
                prompt_kind: crate::session::PromptInputKind::User,
            },
            Some(&target_turn_id),
            InputTurnAdmissionPolicy::AnyTurn,
        )
        .await
        .expect("steered input should be admitted while queued startup is pending");
    let mut abort = tokio::spawn({
        let session = Arc::clone(&session);
        async move {
            session.abort_all_tasks(TurnAbortReason::Interrupted).await;
        }
    });

    assert!(
        timeout(Duration::from_millis(150), &mut abort)
            .await
            .is_err(),
        "forced abort should wait for queued-turn startup beyond its 100 ms ordinary grace window"
    );
    queue_start_permit.publish();
    abort.await.expect("abort task");

    let mut started_agent_queues = Vec::new();
    let mut recorded_inputs = Vec::new();
    let mut saw_aborted_turn = false;
    while recorded_inputs.len() < 2 {
        let event = timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("queued turn lifecycle event")
            .expect("event channel");
        match event.msg {
            EventMsg::TurnStarted(event) => started_agent_queues.push(event.agent_queue),
            EventMsg::ItemCompleted(ItemCompletedEvent {
                item: TurnItem::UserMessage(UserMessageItem { content, .. }),
                ..
            }) => recorded_inputs.push(content),
            EventMsg::TurnAborted(_) => saw_aborted_turn = true,
            _ => {}
        }
    }
    assert_eq!(
        started_agent_queues,
        vec![Some(queue_metadata), None],
        "accepted steer should start exactly one continuation after the queued turn is interrupted"
    );
    assert!(saw_aborted_turn);
    assert_eq!(
        recorded_inputs,
        vec![
            vec![codex_protocol::user_input::UserInput::Text {
                text: "queued task".to_string(),
                text_elements: Vec::new(),
            }],
            vec![codex_protocol::user_input::UserInput::Text {
                text: "steered task".to_string(),
                text_elements: Vec::new(),
            }],
        ],
        "interrupt after delayed queue commit should preserve the queued prompt and its continuation"
    );
    let history = session.clone_history().await;
    let position_of_prompt = |prompt: &str| {
        history
            .raw_items()
            .position(|item| {
                matches!(
                    item,
                    ResponseItem::Message {
                        role,
                        content,
                        ..
                    } if role == "user"
                        && content.iter().any(|content| {
                            matches!(
                                content,
                                ContentItem::InputText { text } if text == prompt
                            )
                        })
                )
            })
            .unwrap_or_else(|| panic!("missing persisted prompt {prompt:?}"))
    };
    assert!(
        position_of_prompt("queued task") < position_of_prompt("steered task"),
        "both admitted prompts should be durable in order"
    );
}

#[tokio::test]
async fn rejected_queue_start_omits_uncommitted_response_handling() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    let submission_id = "queued-submission".to_string();
    let queue_metadata = queue_turn_metadata();
    let mut admission = session.register_input_turn_admission(
        submission_id.clone(),
        InputTurnAdmissionPolicy::Queued,
        Some(queue_metadata.clone()),
    );
    admission.mark_submitted();
    session.resolve_input_turn_admission(
        &submission_id,
        InputTurnAdmissionResolution {
            target_turn_id: "queued-turn".to_string(),
            minimum_event_sequence: 0,
            after_item_id: None,
        },
    );
    let outcome = admission
        .recv()
        .await
        .expect("admission should resolve")
        .expect("admission should succeed");

    drop(
        outcome
            .queue_start_permit
            .expect("queued admission should return its start permit"),
    );

    assert_eq!(
        session.await_agent_queue_turn_metadata("queued-turn").await,
        Some(AgentQueueTurnMetadata {
            response_handling: None,
            ..queue_metadata
        })
    );
}

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
            delivery: None,
            questions: None,
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
fn attributed_agent_input_presentation_is_not_observed_as_a_response() {
    let event = EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: ThreadId::new(),
        turn_id: "turn-1".to_string(),
        item: TurnItem::AgentMessage(AgentMessageItem {
            id: new_attributed_agent_message_response_item_id().to_string(),
            content: vec![AgentMessageContent::Text {
                text: "Agent message from `01900000-0000-7000-8000-000000000001`:\n\nQuestion"
                    .to_string(),
            }],
            phase: Some(MessagePhase::Commentary),
            memory_citation: None,
            delivery: None,
            questions: None,
            sub_agent_completion: None,
        }),
        started_at_ms: Some(0),
        completed_at_ms: 1,
    });

    assert_eq!(agent_response_event(&event, /*sequence*/ 0), None);
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
        agent_queue: None,
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
            delivery: None,
            questions: None,
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
        RolloutItem::ResponseItem(ResponseItemEnvelope::new(response_item.clone())),
    ];
    let paginated_items = vec![
        turn_started,
        RolloutItem::ResponseItem(ResponseItemEnvelope::new(response_item)),
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
            delivery: None,
            questions: None,
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
                agent_queue: None,
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
                agent_queue: None,
            })),
            RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "turn-b".to_string(),
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
                agent_queue: None,
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
            agent_queue: None,
        }),
        EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-b".to_string(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
            agent_queue: None,
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
            agent_queue: None,
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
            agent_queue: None,
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
                delivery: None,
                questions: None,
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
                    delivery: None,
                    questions: None,
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
