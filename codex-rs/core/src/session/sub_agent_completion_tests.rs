use super::*;
use crate::session::SubmissionAdmissionState;
use crate::session::TurnInput;
use crate::session::tests::attach_in_memory_thread_store;
use crate::session::tests::attach_thread_persistence;
use crate::session::tests::make_session_and_context_with_rx;
use crate::state::ActiveTurn;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskContext;
use crate::tasks::SessionTaskResult;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::new_sub_agent_completion_context_response_item_id;
use codex_protocol::protocol::sub_agent_completion_item;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
struct BlockingTask;

fn v1_completion_communication(
    content: &str,
    response_item_id: ResponseItemId,
) -> InterAgentCommunication {
    let mut communication = InterAgentCommunication::new(
        AgentPath::try_from("/root/worker").expect("child agent path"),
        AgentPath::try_from("/root").expect("parent agent path"),
        Vec::new(),
        content.to_string(),
        /*trigger_turn*/ false,
    );
    communication.id = Some(response_item_id);
    communication
}

impl SessionTask for BlockingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.sub_agent_completion_test"
    }

    async fn run(
        self: Arc<Self>,
        _session: Arc<SessionTaskContext>,
        _ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        cancellation_token.cancelled().await;
        Ok(None)
    }
}

#[derive(Clone)]
struct BlockingAbortTask {
    abort_started: Arc<Notify>,
    release_abort: Arc<Notify>,
}

impl SessionTask for BlockingAbortTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.blocking_abort_sub_agent_completion_test"
    }

    async fn run(
        self: Arc<Self>,
        _session: Arc<SessionTaskContext>,
        _ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        cancellation_token.cancelled().await;
        Ok(None)
    }

    async fn abort(&self, _session: Arc<SessionTaskContext>, _ctx: Arc<TurnContext>) {
        self.abort_started.notify_one();
        self.release_abort.notified().await;
    }
}

fn completed_item() -> TurnItem {
    TurnItem::AgentMessage(
        sub_agent_completion_item(
            "/root/reviewer",
            &AgentStatus::Completed(Some("Finished reviewing.".to_string())),
        )
        .expect("terminal status"),
    )
}

fn completed_wait_item(
    sender_thread_id: ThreadId,
    child_thread_id: ThreadId,
    item_id: &str,
    message: &str,
) -> TurnItem {
    TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
        id: item_id.to_string(),
        tool: CollabAgentTool::Wait,
        status: CollabAgentToolCallStatus::Completed,
        observe_commentary: None,
        wake_on_completion: None,
        deadline_at_ms: None,
        sender_thread_id,
        receiver_thread_ids: vec![child_thread_id],
        receiver_agents: Vec::new(),
        prompt: None,
        model: None,
        reasoning_effort: None,
        agents_states: HashMap::from([(
            child_thread_id,
            AgentStatus::Completed(Some(message.to_string())),
        )]),
        completion_presentation_agent_ids: Some(vec![child_thread_id]),
    })
}

async fn emit_completion_and_receive_lifecycle(
    session: &Arc<Session>,
    events: &async_channel::Receiver<Event>,
) -> (Event, Event) {
    let session = Arc::clone(session);
    let history_only_turn_id = uuid::Uuid::now_v7().to_string();
    let emitting = tokio::spawn(async move {
        session
            .emit_turn_item_completed_without_turn_with_history_id(
                completed_item(),
                &history_only_turn_id,
            )
            .await
    });
    let lifecycle = timeout(Duration::from_secs(5), receive_completion_lifecycle(events))
        .await
        .expect("completion lifecycle");
    timeout(Duration::from_secs(5), emitting)
        .await
        .expect("completion emission timeout")
        .expect("completion emission task")
        .expect("emit completion");
    lifecycle
}

async fn abort_tasks(session: &Arc<Session>) {
    timeout(
        Duration::from_secs(5),
        session.abort_all_tasks(TurnAbortReason::Interrupted),
    )
    .await
    .expect("abort tasks");
}

fn publish_agent_status(session: &Session, msg: EventMsg) {
    assert!(session.publish_agent_envelope_status(&Event {
        id: String::new(),
        msg,
    }));
}

async fn receive_completion_lifecycle(events: &async_channel::Receiver<Event>) -> (Event, Event) {
    let started = loop {
        let event = events.recv().await.expect("item started");
        if matches!(
            &event.msg,
            EventMsg::ItemStarted(event)
                if event.item.is_sub_agent_completion_presentation()
        ) {
            break event;
        }
    };
    let completed = loop {
        let event = events.recv().await.expect("item completed");
        if matches!(
            &event.msg,
            EventMsg::ItemCompleted(event)
                if event.item.is_sub_agent_completion_presentation()
        ) {
            break event;
        }
    };
    (started, completed)
}

#[tokio::test]
async fn active_parent_turn_uses_existing_item_lifecycle() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    session
        .spawn_task(Arc::clone(&turn_context), Vec::new(), BlockingTask)
        .await;

    let (started, completed) = emit_completion_and_receive_lifecycle(&session, &events).await;
    let EventMsg::ItemStarted(started) = started.msg else {
        panic!("expected item started");
    };
    let EventMsg::ItemCompleted(completed) = completed.msg else {
        panic!("expected item completed");
    };
    assert_eq!(started.turn_id, turn_context.sub_id);
    assert_eq!(completed.turn_id, turn_context.sub_id);
    assert_eq!(started.item.id(), completed.item.id());

    abort_tasks(&session).await;
}

#[tokio::test]
async fn active_parent_completion_does_not_build_a_discarded_turn_context() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    let mut marker = turn_context.model_info.clone();
    marker.slug = "active-turn-marker".to_string();
    session.services.thread_extension_data.insert(marker);
    session
        .spawn_task(Arc::clone(&turn_context), Vec::new(), BlockingTask)
        .await;

    let _ = emit_completion_and_receive_lifecycle(&session, &events).await;

    assert_eq!(
        session
            .services
            .thread_extension_data
            .get::<ModelInfo>()
            .expect("model info")
            .slug
            .as_str(),
        "active-turn-marker"
    );
    abort_tasks(&session).await;
}

#[tokio::test]
async fn failed_primary_persistence_does_not_commit_delivery_ownership() {
    let (mut session, turn_context, events) = make_session_and_context_with_rx().await;
    attach_thread_persistence(Arc::get_mut(&mut session).expect("unique session")).await;
    let prior_item = completed_wait_item(
        session.thread_id,
        ThreadId::new(),
        "reused-wait-call",
        "prior child done",
    );
    session
        .live_thread()
        .expect("live thread")
        .append_items_and_flush_canonical(&[RolloutItem::EventMsg(EventMsg::ItemCompleted(
            ItemCompletedEvent {
                thread_id: session.thread_id,
                turn_id: "prior-turn".to_string(),
                item: prior_item,
                started_at_ms: None,
                completed_at_ms: now_unix_timestamp_ms(),
            },
        ))])
        .await
        .expect("persist prior wait with reused call ID");
    session
        .live_thread()
        .expect("live thread")
        .shutdown()
        .await
        .expect("shutdown persistence");
    let current_item = completed_wait_item(
        session.thread_id,
        ThreadId::new(),
        "reused-wait-call",
        "current child done",
    );
    let committed = Arc::new(AtomicBool::new(false));
    let committed_after_delivery = Arc::clone(&committed);

    session
        .emit_turn_item_completed_with_primary_delivery(
            turn_context.as_ref(),
            current_item,
            move || {
                committed_after_delivery.store(true, Ordering::Release);
            },
        )
        .await;

    assert!(!committed.load(Ordering::Acquire));
    assert!(matches!(
        events.recv().await.expect("live completion").msg,
        EventMsg::ItemCompleted(_)
    ));
}

#[tokio::test]
async fn committed_primary_persistence_failure_commits_delivery_ownership() {
    let (mut session, turn_context, events) = make_session_and_context_with_rx().await;
    let store =
        attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique session")).await;
    let child_thread_id = ThreadId::new();
    let item = completed_wait_item(
        session.thread_id,
        child_thread_id,
        "wait-call",
        "child done",
    );
    store
        .fail_next_operation(
            codex_thread_store::InMemoryThreadStoreFailure::SubAgentCompletionPresentationFlush,
        )
        .await;
    let committed = Arc::new(AtomicBool::new(false));
    let committed_after_delivery = Arc::clone(&committed);

    session
        .emit_turn_item_completed_with_primary_delivery(
            turn_context.as_ref(),
            item.clone(),
            move || {
                committed_after_delivery.store(true, Ordering::Release);
            },
        )
        .await;

    assert!(committed.load(Ordering::Acquire));
    let EventMsg::ItemCompleted(delivered) = events.recv().await.expect("live completion").msg
    else {
        panic!("expected item completed");
    };
    assert_eq!(
        serde_json::to_value(&delivered.item).expect("serialize delivered item"),
        serde_json::to_value(&item).expect("serialize expected item")
    );
    let persisted = session
        .persisted_sub_agent_completion_presentation("wait-call", &turn_context.sub_id)
        .await
        .expect("load committed presentation");
    assert_eq!(
        serde_json::to_value(persisted.item_completed.map(|event| event.item))
            .expect("serialize persisted item"),
        serde_json::to_value(Some(item)).expect("serialize expected item")
    );
}

#[tokio::test]
async fn active_turn_abort_preserves_durable_v1_subagent_notification() {
    let (mut session, turn_context, events) = make_session_and_context_with_rx().await;
    attach_thread_persistence(Arc::get_mut(&mut session).expect("unique session")).await;
    session
        .spawn_task(Arc::clone(&turn_context), Vec::new(), BlockingTask)
        .await;
    let notification = "<subagent_notification>durable child result</subagent_notification>";

    session
        .record_sub_agent_notification_with_observation_commit(
            v1_completion_communication(
                notification,
                new_sub_agent_completion_context_response_item_id(),
            ),
            CompletionSubmissionAdmission::Ordinary,
            Vec::new(),
        )
        .await
        .expect("persist notification");
    let _raw_response_item = events.recv().await.expect("raw response item");
    abort_tasks(&session).await;

    assert!(
        session
            .clone_history()
            .await
            .raw_items()
            .iter()
            .any(|item| matches!(
                item,
                ResponseItem::AgentMessage { content, .. }
                    if content.iter().any(|content| matches!(
                        content,
                        AgentMessageInputContent::InputText { text } if text == notification
                    ))
            ))
    );
}

#[tokio::test]
async fn observed_notification_try_admission_does_not_wait_for_rollback() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    *session
        .submission_admission
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        SubmissionAdmissionState::RollbackPending;

    let result = timeout(
        Duration::from_millis(50),
        session.record_sub_agent_notification_with_observation_commit(
            v1_completion_communication(
                "child done",
                new_sub_agent_completion_context_response_item_id(),
            ),
            CompletionSubmissionAdmission::Ordinary,
            Vec::new(),
        ),
    )
    .await
    .expect("try admission should not wait for rollback");

    assert!(matches!(result, Err(ThreadStoreError::Internal { .. })));
}

#[tokio::test]
async fn taskless_parent_transition_delays_completion_lifecycle() {
    let (session, _turn_context, events) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let emitting_session = Arc::clone(&session);
    let history_only_turn_id = uuid::Uuid::now_v7().to_string();
    let emit = tokio::spawn(async move {
        emitting_session
            .emit_turn_item_completed_without_turn_with_history_id(
                completed_item(),
                &history_only_turn_id,
            )
            .await
    });

    assert!(
        timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err()
    );
    *session.active_turn.lock().await = None;
    session.active_turn_transition.notify_waiters();

    let started = events.recv().await.expect("item started");
    let completed = events.recv().await.expect("item completed");
    emit.await
        .expect("completion emission task")
        .expect("emit completion");
    let EventMsg::ItemStarted(started) = started.msg else {
        panic!("expected item started");
    };
    let EventMsg::ItemCompleted(completed) = completed.msg else {
        panic!("expected item completed");
    };
    assert_eq!(started.turn_id, completed.turn_id);
    assert_eq!(started.item.id(), completed.item.id());
}

#[tokio::test]
async fn abort_cleanup_delays_completion_lifecycle_until_the_turn_is_cleared() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    let abort_started = Arc::new(Notify::new());
    let release_abort = Arc::new(Notify::new());
    session
        .spawn_task(
            Arc::clone(&turn_context),
            Vec::new(),
            BlockingAbortTask {
                abort_started: Arc::clone(&abort_started),
                release_abort: Arc::clone(&release_abort),
            },
        )
        .await;
    let abort = {
        let session = Arc::clone(&session);
        tokio::spawn(async move {
            session.abort_all_tasks(TurnAbortReason::Interrupted).await;
        })
    };
    timeout(Duration::from_secs(5), abort_started.notified())
        .await
        .expect("abort cleanup start");
    {
        let active_turn = session.active_turn.lock().await;
        assert!(
            active_turn
                .as_ref()
                .is_some_and(|active_turn| active_turn.task.is_none())
        );
    }
    let emitting_session = Arc::clone(&session);
    let history_only_turn_id = uuid::Uuid::now_v7().to_string();
    let emit = tokio::spawn(async move {
        emitting_session
            .emit_turn_item_completed_without_turn_with_history_id(
                completed_item(),
                &history_only_turn_id,
            )
            .await
    });

    assert!(
        timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err()
    );
    release_abort.notify_one();
    timeout(Duration::from_secs(5), abort)
        .await
        .expect("abort cleanup completion")
        .expect("abort task");
    let _lifecycle = timeout(
        Duration::from_secs(5),
        receive_completion_lifecycle(&events),
    )
    .await
    .expect("completion lifecycle after abort cleanup");
    timeout(Duration::from_secs(5), emit)
        .await
        .expect("completion emission after abort cleanup")
        .expect("completion emission task")
        .expect("emit completion");
}

#[tokio::test]
async fn idle_completion_turn_ids_are_unique_across_sessions() {
    let (first_session, _turn_context, first_events) = make_session_and_context_with_rx().await;
    let (first_started, _first_completed) =
        emit_completion_and_receive_lifecycle(&first_session, &first_events).await;
    let EventMsg::ItemStarted(first_started) = first_started.msg else {
        panic!("expected first item started");
    };

    let (second_session, _turn_context, second_events) = make_session_and_context_with_rx().await;
    let (second_started, _second_completed) =
        emit_completion_and_receive_lifecycle(&second_session, &second_events).await;
    let EventMsg::ItemStarted(second_started) = second_started.msg else {
        panic!("expected second item started");
    };

    assert_ne!(first_started.turn_id, second_started.turn_id);
    for turn_id in [first_started.turn_id, second_started.turn_id] {
        let turn_id = uuid::Uuid::parse_str(&turn_id).expect("UUID completion turn ID");
        assert_eq!(turn_id.get_version(), Some(uuid::Version::SortRand));
    }
}

#[tokio::test]
async fn canonical_completion_reuses_a_previously_committed_history_item() {
    let (mut session, _turn_context, events) = make_session_and_context_with_rx().await;
    attach_thread_persistence(Arc::get_mut(&mut session).expect("unique session")).await;
    let history_only_turn_id = uuid::Uuid::now_v7().to_string();
    let item = completed_item();
    let item_id = item.id();
    let completed_at_ms = now_unix_timestamp_ms();
    let persisted_completion = EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: session.thread_id,
        turn_id: history_only_turn_id.clone(),
        item: item.clone(),
        started_at_ms: None,
        completed_at_ms,
    });
    session
        .live_thread()
        .expect("live thread")
        .append_items_and_flush_canonical(&[
            RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: history_only_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
            })),
            RolloutItem::EventMsg(persisted_completion),
            RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: history_only_turn_id.clone(),
                last_agent_message: None,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            })),
        ])
        .await
        .expect("persist completion before reported failure");

    let emitting_session = Arc::clone(&session);
    let emitting_history_only_turn_id = history_only_turn_id.clone();
    let emitting = tokio::spawn(async move {
        emitting_session
            .emit_turn_item_completed_without_turn_with_history_id(
                item,
                &emitting_history_only_turn_id,
            )
            .await
    });
    let _lifecycle = receive_completion_lifecycle(&events).await;
    emitting
        .await
        .expect("completion emission task")
        .expect("reconcile committed completion");

    let history = session
        .live_thread()
        .expect("live thread")
        .load_history(/*include_archived*/ false)
        .await
        .expect("load history");
    assert_eq!(
        history
            .items
            .iter()
            .filter(|rollout_item| matches!(
                rollout_item,
                RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                    if event.item.id() == item_id
            ))
            .count(),
        1
    );
}

#[tokio::test]
async fn v1_completion_context_reuses_a_previously_committed_response_item() {
    let (mut session, _turn_context, events) = make_session_and_context_with_rx().await;
    attach_thread_persistence(Arc::get_mut(&mut session).expect("unique session")).await;
    let notification = "<subagent_notification>durable child result</subagent_notification>";
    let response_item_id = new_sub_agent_completion_context_response_item_id();
    let communication = v1_completion_communication(notification, response_item_id.clone());
    let response_item = communication.to_model_input_item();
    session
        .live_thread()
        .expect("live thread")
        .append_items_and_flush_canonical(&[
            RolloutItem::InterAgentCommunicationMetadata {
                trigger_turn: false,
            },
            RolloutItem::ResponseItem(response_item),
        ])
        .await
        .expect("persist completion context before reported failure");

    session
        .record_sub_agent_notification_with_observation_commit(
            communication,
            CompletionSubmissionAdmission::Ordinary,
            Vec::new(),
        )
        .await
        .expect("reconcile committed completion context");
    let _raw_response_item = events.recv().await.expect("raw response item");

    let history = session
        .live_thread()
        .expect("live thread")
        .load_history(/*include_archived*/ false)
        .await
        .expect("load history");
    assert_eq!(
        history
            .items
            .iter()
            .filter(|rollout_item| matches!(
                rollout_item,
                RolloutItem::ResponseItem(response_item)
                    if response_item.id() == Some(&response_item_id)
            ))
            .count(),
        1
    );
    let persisted_response_item = history
        .items
        .iter()
        .find_map(|rollout_item| match rollout_item {
            RolloutItem::ResponseItem(response_item)
                if response_item.id() == Some(&response_item_id) =>
            {
                Some(response_item)
            }
            _ => None,
        })
        .expect("persisted response item");
    assert_eq!(
        session.clone_history().await.raw_items(),
        std::slice::from_ref(persisted_response_item)
    );
}

#[tokio::test]
async fn terminal_presentation_rearms_only_after_running_status() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    let parent_thread_id = ThreadId::new();
    let parent =
        crate::agent::control::SessionPresentationId::new(parent_thread_id, uuid::Uuid::now_v7());
    let _watcher_registration = session
        .services
        .agent_control
        .register_completion_watcher(session.presentation_id(), parent)
        .expect("watcher registration");
    session.agent_status.send_replace(AgentStatus::Running);

    let first = session.record_sub_agent_terminal_presentation(
        parent_thread_id,
        "turn-1",
        AgentStatus::Completed(Some("done".to_string())),
        TerminalPresentationDelivery::Direct,
    );
    let teardown = session.record_sub_agent_terminal_presentation(
        parent_thread_id,
        "shutdown",
        AgentStatus::Shutdown,
        TerminalPresentationDelivery::Watcher,
    );

    assert!(first.is_some());
    assert!(teardown.is_none());

    session.agent_status.send_replace(AgentStatus::Running);
    let next = session.record_sub_agent_terminal_presentation(
        parent_thread_id,
        "turn-2",
        AgentStatus::Completed(Some("done again".to_string())),
        TerminalPresentationDelivery::Direct,
    );
    assert!(next.is_some());
}

#[tokio::test]
async fn historical_terminal_does_not_replace_a_newer_turn_status() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    let parent_thread_id = ThreadId::new();
    let parent =
        crate::agent::control::SessionPresentationId::new(parent_thread_id, uuid::Uuid::now_v7());
    let _watcher_registration = session
        .services
        .agent_control
        .register_completion_watcher(session.presentation_id(), parent)
        .expect("watcher registration");
    assert!(session.begin_agent_response_turn("current-turn"));

    let presentation = session.record_sub_agent_terminal_presentation(
        parent_thread_id,
        "historical-turn",
        AgentStatus::Completed(Some("historical result".to_string())),
        TerminalPresentationDelivery::Direct,
    );
    assert!(presentation.is_some());
    assert_eq!(session.agent_status.borrow().clone(), AgentStatus::Running);

    assert!(session.publish_agent_response_event_and_status(&Event {
        id: "historical-turn".to_string(),
        msg: EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "historical-turn".to_string(),
            last_agent_message: Some("historical result".to_string()),
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        }),
    }));
    let (snapshot, subscription) = session.subscribe_agent_responses();
    drop(subscription);
    assert_eq!(
        (
            snapshot.active_turn_id,
            snapshot.last_terminal,
            snapshot.status,
        ),
        (
            Some("current-turn".to_string()),
            Some((
                "historical-turn".to_string(),
                AgentStatus::Completed(Some("historical result".to_string())),
            )),
            AgentStatus::Running,
        )
    );
}

#[tokio::test]
async fn removal_settles_a_prepared_terminal_response_snapshot() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    let parent_thread_id = ThreadId::new();
    let parent =
        crate::agent::control::SessionPresentationId::new(parent_thread_id, uuid::Uuid::now_v7());
    let _watcher_registration = session
        .services
        .agent_control
        .register_completion_watcher(session.presentation_id(), parent)
        .expect("watcher registration");
    assert!(session.begin_agent_response_turn("prepared-turn"));
    assert!(
        session
            .record_sub_agent_terminal_presentation(
                parent_thread_id,
                "prepared-turn",
                AgentStatus::Completed(Some("prepared result".to_string())),
                TerminalPresentationDelivery::Direct,
            )
            .is_some()
    );

    let (prepared_snapshot, prepared_subscription) = session.subscribe_agent_responses();
    drop(prepared_subscription);
    assert_eq!(
        (
            prepared_snapshot.active_turn_id,
            prepared_snapshot.last_terminal,
            prepared_snapshot.status,
        ),
        (
            Some("prepared-turn".to_string()),
            None,
            AgentStatus::Completed(Some("prepared result".to_string())),
        )
    );

    session.prepare_for_thread_removal();
    session.finish_thread_removal();

    let (removed_snapshot, mut removed_subscription) = session.subscribe_agent_responses();
    assert_eq!(
        (
            removed_snapshot.active_turn_id,
            removed_snapshot.last_terminal,
            removed_snapshot.status,
        ),
        (
            None,
            Some((
                "prepared-turn".to_string(),
                AgentStatus::Completed(Some("prepared result".to_string())),
            )),
            AgentStatus::Completed(Some("prepared result".to_string())),
        )
    );
    assert_eq!(removed_subscription.recv().await, None);
}

#[tokio::test]
async fn disarmed_removal_closes_observer_streams_for_retained_session() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    let (_snapshot, mut response_subscription) = session.subscribe_agent_responses();
    let (_status, mut terminal_status_subscription) = session.subscribe_terminal_status();
    session
        .agent_status
        .send_replace(AgentStatus::Completed(Some(
            "completed before unload".to_string(),
        )));
    let presentation_disarm = session.disarm_terminal_presentation();
    presentation_disarm.commit();

    session.prepare_for_thread_removal();
    session.finish_thread_removal();

    assert_eq!(
        timeout(Duration::from_secs(1), response_subscription.recv())
            .await
            .expect("removed response stream should close"),
        None
    );
    assert_eq!(terminal_status_subscription.recv().await, None);
    let (_late_snapshot, mut late_response_subscription) = session.subscribe_agent_responses();
    let (_late_status, mut late_terminal_status_subscription) = session.subscribe_terminal_status();
    assert_eq!(late_response_subscription.recv().await, None);
    assert_eq!(late_terminal_status_subscription.recv().await, None);
    assert_eq!(
        session.agent_status.borrow().clone(),
        AgentStatus::Completed(Some("completed before unload".to_string()))
    );
}

#[tokio::test]
async fn watcher_terminal_is_recorded_for_every_observer_before_status_is_final() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    let first_parent_thread_id = ThreadId::new();
    let first_parent = crate::agent::control::SessionPresentationId::new(
        first_parent_thread_id,
        uuid::Uuid::now_v7(),
    );
    let second_parent =
        crate::agent::control::SessionPresentationId::new(ThreadId::new(), uuid::Uuid::now_v7());
    let child = session.presentation_id();
    let _first_registration = session
        .services
        .agent_control
        .register_completion_watcher(child, first_parent)
        .expect("first watcher registration");
    let _second_registration = session
        .services
        .agent_control
        .register_completion_watcher(child, second_parent)
        .expect("second watcher registration");
    session.agent_status.send_replace(AgentStatus::Running);

    let presentation = session.record_sub_agent_terminal_presentation(
        first_parent_thread_id,
        "turn-1",
        AgentStatus::Completed(Some("done".to_string())),
        TerminalPresentationDelivery::Watcher,
    );

    assert!(presentation.is_none());
    assert_eq!(
        (
            session
                .services
                .agent_control
                .take_watcher_terminal_presentation(first_parent, child)
                .map(|terminal| terminal.status),
            session
                .services
                .agent_control
                .take_watcher_terminal_presentation(second_parent, child)
                .map(|terminal| terminal.status),
            session.agent_status.borrow().clone(),
        ),
        (
            Some(AgentStatus::Completed(Some("done".to_string()))),
            Some(AgentStatus::Completed(Some("done".to_string()))),
            AgentStatus::Completed(Some("done".to_string())),
        )
    );
}

#[tokio::test]
async fn terminal_status_is_preserved_until_the_next_turn_starts() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    let terminal_error = ErrorEvent {
        message: "child failed".to_string(),
        codex_error_info: Some(CodexErrorInfo::BadRequest),
    };

    publish_agent_status(&session, EventMsg::Error(terminal_error.clone()));
    publish_agent_status(
        &session,
        EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: Some("incorrect success".to_string()),
            error: Some(terminal_error),
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        }),
    );
    publish_agent_status(&session, EventMsg::ShutdownComplete);

    assert_eq!(
        session.agent_status.borrow().clone(),
        AgentStatus::Errored("child failed".to_string())
    );

    publish_agent_status(
        &session,
        EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-2".to_string(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        }),
    );
    publish_agent_status(
        &session,
        EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-2".to_string(),
            last_agent_message: Some("done".to_string()),
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        }),
    );

    assert_eq!(
        session.agent_status.borrow().clone(),
        AgentStatus::Completed(Some("done".to_string()))
    );
}

#[tokio::test]
async fn terminal_status_subscription_identifies_the_completed_turn() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    let (_status, mut subscription) = session.subscribe_terminal_status();
    let status = AgentStatus::Completed(Some("done".to_string()));

    publish_agent_status(
        &session,
        EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: Some("done".to_string()),
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        }),
    );

    assert_eq!(
        subscription.recv().await,
        Some(TerminalStatusEvent {
            turn_id: Some("turn-1".to_string()),
            status,
        })
    );
}

#[tokio::test]
async fn terminal_status_snapshot_recovers_turn_identity_from_rollout_state() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    let status = AgentStatus::Completed(Some("restored".to_string()));
    session
        .response_observation_state
        .lock()
        .expect("response observation state")
        .last_terminal = Some(("restored-turn".to_string(), status.clone()));
    session.agent_status.send_replace(status.clone());

    let (snapshot, _subscription) = session.subscribe_terminal_status();

    assert_eq!(
        snapshot,
        TerminalStatusEvent {
            turn_id: Some("restored-turn".to_string()),
            status,
        }
    );
}

#[tokio::test]
async fn dropped_terminal_status_subscription_unregisters_immediately() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    let (_status, subscription) = session.subscribe_terminal_status();
    assert_eq!(
        session
            .terminal_status_subscribers
            .lock()
            .expect("terminal subscriber lock")
            .len(),
        1
    );

    drop(subscription);

    assert!(
        session
            .terminal_status_subscribers
            .lock()
            .expect("terminal subscriber lock")
            .is_empty()
    );
}

#[tokio::test]
async fn terminal_status_subscription_disconnects_when_session_drops() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    let (_status, mut subscription) = session.subscribe_terminal_status();

    drop(session);

    assert_eq!(
        subscription.recv().await.map(|event| event.status),
        Some(AgentStatus::NotFound)
    );
    assert_eq!(subscription.recv().await, None);
}
