use super::*;
use crate::agent::response_observation::FinalResponseObservation;
use pretty_assertions::assert_eq;

fn queued_turn(
    id: &str,
    control: LocalAgentControl,
    source: SessionPresentationId,
    target_thread_id: ThreadId,
    message: &str,
) -> QueuedAgentTurn {
    QueuedAgentTurn {
        id: Uuid::parse_str(id).expect("valid queue id"),
        control,
        source,
        target_thread_id,
        input: AgentControlInput::User(vec![UserInput::Text {
            text: message.to_string(),
            text_elements: Vec::new(),
        }]),
        start_options: Default::default(),
        response_observation: ResponseObservationPolicy::from_turn_parts(
            /*commentary*/ false,
            FinalResponseObservation::Passive,
            /*target_messages*/ false,
            /*queue_input*/ true,
        ),
        task_preview: None,
        authored_selector: Some("2".to_string()),
        target_message_wake: None,
    }
}

fn queued_turn_view(turn: &QueuedAgentTurn) -> QueuedAgentTurnView {
    QueuedAgentTurnView {
        id: turn.id,
        source_thread_id: turn.source.thread_id,
        target_thread_id: turn.target_thread_id,
        input: turn.input.presentation().to_vec(),
        prompt_preview: crate::agent::control::render_input_preview(turn.input.presentation()),
        response_observation: turn.response_observation,
        authored_selector: turn.authored_selector.clone(),
    }
}

#[test]
fn list_preserves_target_fifo_and_source_close_cancels_pending_turns() {
    let session_id = SessionId::default();
    let control = LocalAgentControl::default().with_session_id(session_id, /*max_threads*/ 20);
    let source = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let target_thread_id = ThreadId::new();
    let first = queued_turn(
        "00000000-0000-7000-8000-000000000001",
        control.clone(),
        source,
        target_thread_id,
        "first",
    );
    let second = queued_turn(
        "00000000-0000-7000-8000-000000000002",
        control,
        source,
        target_thread_id,
        "second",
    );
    let queue = AgentTurnQueue::default();

    assert!(queue.enqueue(first.clone()));
    assert!(!queue.enqueue(second.clone()));
    assert_eq!(
        queue.list_for_root(session_id),
        vec![queued_turn_view(&first), queued_turn_view(&second)]
    );
    assert!(queue.has_pending_involving(source.thread_id));
    assert!(queue.has_pending_involving(target_thread_id));

    queue.cancel_for_threads([source.thread_id]);
    assert_eq!(queue.list_for_root(session_id), Vec::new());
    assert!(!queue.has_pending(target_thread_id));
    assert!(!queue.has_pending_involving(source.thread_id));
    assert!(!queue.has_pending_involving(target_thread_id));
}

#[test]
fn cancellation_can_remove_in_flight_queue_entry_until_admission_begins() {
    let session_id = SessionId::default();
    let control = LocalAgentControl::default().with_session_id(session_id, /*max_threads*/ 20);
    let source = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let target_thread_id = ThreadId::new();
    let turn = queued_turn(
        "00000000-0000-7000-8000-000000000001",
        control,
        source,
        target_thread_id,
        "pending",
    );
    let queue = AgentTurnQueue::default();
    assert!(queue.enqueue(turn.clone()));
    assert_eq!(
        queue.take_front(target_thread_id).map(|turn| turn.id),
        Some(turn.id)
    );
    assert_eq!(
        queue.list_for_root(session_id),
        vec![queued_turn_view(&turn)]
    );

    assert!(queue.cancel(session_id, turn.id));

    assert!(!queue.has_pending(target_thread_id));
    assert!(!queue.begin_admission(target_thread_id, turn.id));

    let admitted = queued_turn(
        "00000000-0000-7000-8000-000000000002",
        turn.control,
        source,
        target_thread_id,
        "admitted",
    );
    queue.enqueue(admitted.clone());
    assert_eq!(
        queue.take_front(target_thread_id).map(|turn| turn.id),
        Some(admitted.id)
    );
    assert!(queue.begin_admission(target_thread_id, admitted.id));

    assert!(!queue.cancel(session_id, admitted.id));
    queue.cancel_for_threads([target_thread_id]);

    assert!(queue.has_pending(target_thread_id));
    queue.finish_front(target_thread_id, admitted.id);
    assert!(!queue.has_pending(target_thread_id));
}

#[tokio::test]
async fn source_lifecycle_cancellation_wins_before_queue_admission_claim() {
    let session_id = SessionId::default();
    let control = LocalAgentControl::default().with_session_id(session_id, /*max_threads*/ 20);
    let source = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let target_thread_id = ThreadId::new();
    let turn = queued_turn(
        "00000000-0000-7000-8000-000000000001",
        control,
        source,
        target_thread_id,
        "pending",
    );
    let queue = Arc::new(AgentTurnQueue::default());
    assert!(queue.enqueue(turn.clone()));
    assert_eq!(
        queue.take_front(target_thread_id).map(|turn| turn.id),
        Some(turn.id)
    );

    let lifecycle_guard = queue.acquire_source_admission(source.thread_id).await;
    let (attempted_tx, attempted_rx) = tokio::sync::oneshot::channel();
    let worker_queue = Arc::clone(&queue);
    let worker = tokio::spawn(async move {
        attempted_tx.send(()).expect("signal admission attempt");
        let _source_guard = worker_queue
            .acquire_source_admission(source.thread_id)
            .await;
        worker_queue.begin_admission(target_thread_id, turn.id)
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), attempted_rx)
        .await
        .expect("bounded worker admission attempt")
        .expect("worker admission attempt");

    queue.cancel_for_threads([source.thread_id]);
    drop(lifecycle_guard);

    assert!(
        !tokio::time::timeout(std::time::Duration::from_secs(5), worker)
            .await
            .expect("bounded worker completion")
            .expect("worker should finish")
    );
    assert!(!queue.has_pending(target_thread_id));
}

#[test]
fn proven_not_submitted_restores_fifo_but_claimed_input_cannot_be_deleted() {
    let root = SessionId::default();
    let control = LocalAgentControl::default().with_session_id(root, /*max_threads*/ 20);
    let source = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let target = ThreadId::new();
    let first = queued_turn(
        "00000000-0000-7000-8000-000000000001",
        control.clone(),
        source,
        target,
        "first",
    );
    let second = queued_turn(
        "00000000-0000-7000-8000-000000000002",
        control,
        source,
        target,
        "second",
    );
    let queue = AgentTurnQueue::default();
    queue.enqueue(first.clone());
    queue.enqueue(second.clone());
    assert_eq!(
        queue.take_front(target).map(|entry| entry.id),
        Some(first.id)
    );
    assert!(queue.begin_admission(target, first.id));
    assert!(!queue.cancel(root, first.id));
    queue.restore_front_after_not_submitted(target, first.id);
    assert_eq!(
        queue.list_for_root(root),
        vec![queued_turn_view(&first), queued_turn_view(&second)]
    );
    assert_eq!(
        queue.take_front(target).map(|entry| entry.id),
        Some(first.id)
    );
    assert!(queue.list_for_root(SessionId::default()).is_empty());
}
