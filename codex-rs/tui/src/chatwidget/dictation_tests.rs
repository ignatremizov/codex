use super::*;
use crate::chatwidget::realtime::RealtimeConversationPhase;
use crate::chatwidget::tests::make_chatwidget_manual_with_sender;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn stale_result_cannot_replace_a_new_recording_placeholder() {
    let (mut chat, _tx, _events, _ops) = make_chatwidget_manual_with_sender().await;
    let element = chat.bottom_pane.begin_dictation();
    let cancel = CancellationToken::new();
    chat.dictation = Some(Session {
        generation: 42,
        element,
        thread: chat.thread_id(),
        stop: CancellationToken::new(),
        cancel: cancel.clone(),
    });
    let before = chat.composer_text_with_pending();
    chat.on_dictation_update(41, element, Update::Finished);
    assert_eq!(chat.composer_text_with_pending(), before);
    assert!(!cancel.is_cancelled());
    chat.on_dictation_update(42, element, Update::Finished);
    assert!(cancel.is_cancelled());
    assert_eq!(chat.composer_text_with_pending(), "");
}

#[tokio::test]
async fn realtime_starting_excludes_dictation_before_preflight() {
    let (mut chat, _tx, _events, _ops) = make_chatwidget_manual_with_sender().await;
    chat.local_settings.voice_transcription_enabled = true;
    chat.realtime_conversation.phase = RealtimeConversationPhase::Starting;
    chat.toggle_dictation();
    assert!(chat.dictation.is_none());
    assert_eq!(chat.composer_text_with_pending(), "");
}

#[tokio::test]
async fn thread_snapshot_cancels_recording_without_persisting_placeholder() {
    let (mut chat, _tx, _events, _ops) = make_chatwidget_manual_with_sender().await;
    let element = chat.bottom_pane.begin_dictation();
    let cancel = CancellationToken::new();
    chat.dictation = Some(Session {
        generation: 42,
        element,
        thread: chat.thread_id(),
        stop: CancellationToken::new(),
        cancel: cancel.clone(),
    });
    let _ = chat.capture_thread_input_state();
    assert!(cancel.is_cancelled());
    assert_eq!(chat.composer_text_with_pending(), "");
}

#[tokio::test]
async fn late_realtime_reservation_after_stop_is_released_without_starting_helper() {
    let (mut chat, _tx, _events, _ops) = make_chatwidget_manual_with_sender().await;
    let reservation = MicReservation::default();
    let lease = reservation.acquire().expect("lease");
    chat.on_realtime_microphone_ready(codex_protocol::ThreadId::new(), 42, Ok(lease));
    assert!(reservation.acquire().is_some());
    assert!(!chat.realtime_conversation_is_running());
}

#[tokio::test]
async fn completed_text_stays_editable_and_deleted_placeholder_cancels() {
    let (mut chat, _tx, _events, _ops) = make_chatwidget_manual_with_sender().await;
    chat.local_settings.voice_transcription_enabled = true;
    if !chat.dictation_enabled() {
        return;
    }
    let element = chat.bottom_pane.begin_dictation();
    let cancel = CancellationToken::new();
    chat.dictation = Some(Session {
        generation: 42,
        element,
        thread: chat.thread_id(),
        stop: CancellationToken::new(),
        cancel: cancel.clone(),
    });
    let slots = Arc::new(tokio::sync::Semaphore::new(/*permits*/ 1));
    chat.on_dictation_update(
        42,
        element,
        Update::Chunk {
            result: Ok("日本語".repeat(10_000)),
            reservation: slots.clone().try_acquire_owned().expect("slot"),
        },
    );
    assert_eq!(slots.available_permits(), 1);
    assert!(
        chat.composer_text_with_pending()
            .starts_with(&"日本語".repeat(10_000))
    );
    chat.bottom_pane.finish_dictation(element);
    chat.on_dictation_update(42, element, Update::Recording);
    assert!(cancel.is_cancelled());
    assert_eq!(
        chat.composer_text_with_pending(),
        format!("{} ", "日本語".repeat(10_000))
    );
}

#[cfg(not(all(target_os = "linux", target_env = "musl")))]
#[tokio::test]
async fn canceled_preflight_does_not_start_capture_or_publish_results() {
    let (chat, _tx, _events, _ops) = make_chatwidget_manual_with_sender().await;
    let reservation = MicReservation::default();
    let lease = reservation.acquire().expect("lease");
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    crate::dictation::session::run(
        Arc::new(chat.config.clone()),
        /*generation*/ 42,
        /*element*/ 1,
        CancellationToken::new(),
        cancel,
        lease,
        crate::app_event_sender::AppEventSender::new(tx),
    )
    .await;
    assert!(reservation.acquire().is_some());
    assert!(events.try_recv().is_err());
}
