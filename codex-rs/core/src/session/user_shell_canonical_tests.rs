//! A model queue is only a continuation hint, never the sole owner of completed shell output.

use super::*;
use crate::session::tests::attach_in_memory_thread_store;
use codex_history::RolloutItem;
use codex_protocol::protocol::TurnAbortReason;
use codex_thread_store::InMemoryThreadStoreFailure;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::PersistContext;
use codex_thread_store::ThreadStore;
use pretty_assertions::assert_eq;
use std::time::Duration;

#[tokio::test]
async fn completed_shell_context_survives_abandoned_active_and_next_turn_signals() {
    for accepting in [true, false] {
        let (mut session, turn, _events) = make_session_and_context_with_rx().await;
        let store =
            attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique session"))
                .await;
        session
            .spawn_task(Arc::clone(&turn), Vec::new(), ContinuingTask)
            .await;
        let turn_state = {
            let active = session.active_turn.lock().await;
            let active = active.as_ref().expect("active task");
            active
                .task
                .as_ref()
                .expect("task owner")
                .accepting_pending_input
                .store(accepting, Ordering::Release);
            Arc::clone(&active.turn_state)
        };
        let completion = session
            .submission_admission
            .try_accept_completion_delivery()
            .expect("command owns its publication");
        assert_eq!(
            session
                .deliver_user_shell_result(
                    result_item(),
                    &turn,
                    UserShellCommandFinalDelivery::Wake,
                    &completion,
                )
                .await
                .expect("acknowledged result"),
            !accepting
        );
        let installed = session.clone_history().await.into_annotated_items();
        assert_eq!(installed.len(), 1);
        let canonical = store
            .load_history(LoadThreadHistoryParams {
                thread_id: session.thread_id,
                include_archived: false,
            })
            .await
            .expect("canonical result precedes consumption")
            .items
            .into_iter()
            .filter_map(|item| match item {
                RolloutItem::ResponseItem(item) => Some(item),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(canonical, installed);

        session.submission_admission.close_completion_admission();
        drop(completion);
        let abandoned = ActiveTurn {
            turn_state: Arc::clone(&turn_state),
            ..Default::default()
        };
        session.input_queue.clear_pending(&abandoned).await;
        let (signals, _) = session.input_queue.take_queued_items_for_next_turn().await;
        assert_eq!(
            signals,
            if accepting {
                Vec::new()
            } else {
                vec![TurnInput::UserShellContextReady]
            }
        );
        drop(signals);
        assert_eq!(
            session.clone_history().await.into_annotated_items(),
            canonical
        );
        tokio::time::timeout(
            Duration::from_secs(/*secs*/ 5),
            session.submission_admission.drain_accepted_completions(),
        )
        .await
        .expect("no receipt waits for a discarded model turn");
        tokio::time::timeout(
            Duration::from_secs(/*secs*/ 5),
            session.abort_all_tasks(TurnAbortReason::Interrupted),
        )
        .await
        .expect("release the independent test task");
    }
}

#[tokio::test]
async fn consuming_shell_continuation_does_not_append_or_reclassify_the_result() {
    let (mut session, turn, _events) = make_session_and_context_with_rx().await;
    let store =
        attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique session")).await;
    let completion = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("accepted command");
    assert!(
        session
            .deliver_user_shell_result(
                result_item(),
                &turn,
                UserShellCommandFinalDelivery::Wake,
                &completion,
            )
            .await
            .expect("canonical result")
    );
    let before = session.clone_history().await.into_annotated_items();
    let writes = store.calls().await.append_items;
    assert!(writes > 0, "the canonical append was exercised");
    let (signals, options) = session.input_queue.take_queued_items_for_next_turn().await;
    assert_eq!(signals, vec![TurnInput::UserShellContextReady]);
    assert_eq!(options.turn_trigger.as_deref(), Some("user_shell_wake"));
    for signal in signals {
        assert!(!signal.is_prompt());
        crate::hook_runtime::record_pending_input(
            &session,
            &turn,
            turn.model_info(),
            signal,
            Vec::new(),
            PersistContext::Standard,
        )
        .await
        .expect("consume scheduling-only input");
    }
    assert_eq!(store.calls().await.append_items, writes);
    assert_eq!(session.clone_history().await.into_annotated_items(), before);
    assert!(serde_json::to_value(TurnInput::UserShellContextReady).is_err());
    assert!(
        serde_json::from_value::<TurnInput>(serde_json::json!("UserShellContextReady")).is_err()
    );
}

#[tokio::test]
async fn uncertain_shell_publication_never_admits_a_wake_or_retries_the_write() {
    for failure in [
        InMemoryThreadStoreFailure::HistoryAppend,
        InMemoryThreadStoreFailure::HistoryFlush,
    ] {
        let (mut session, turn, _events) = make_session_and_context_with_rx().await;
        let store =
            attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique session"))
                .await;
        let completion = session
            .submission_admission
            .try_accept_completion_delivery()
            .expect("accepted command");
        // The shared conversation publisher uses ordinary canonical append-and-flush,
        // not the separate subagent completion operation.
        store.fail_next_operation(failure).await;
        assert!(
            session
                .deliver_user_shell_result(
                    result_item(),
                    &turn,
                    UserShellCommandFinalDelivery::Wake,
                    &completion,
                )
                .await
                .is_err()
        );
        assert!(session.submission_admission.requires_reload());
        assert!(
            session
                .clone_history()
                .await
                .into_annotated_items()
                .is_empty()
        );
        assert_eq!(
            session.input_queue.take_queued_items_for_next_turn().await,
            (Vec::new(), TurnStartOptions::default())
        );
        let writes = store.calls().await.append_items;
        assert_eq!(writes, 1, "one attempted canonical append");
        let persisted = store
            .load_history(LoadThreadHistoryParams {
                thread_id: session.thread_id,
                include_archived: false,
            })
            .await
            .expect("read possible committed payload after a lost acknowledgment")
            .items;
        assert_eq!(
            persisted
                .iter()
                .filter(|item| matches!(item, RolloutItem::ResponseItem(_)))
                .count(),
            usize::from(failure == InMemoryThreadStoreFailure::HistoryFlush),
        );
        assert!(
            session
                .deliver_user_shell_result(
                    result_item(),
                    &turn,
                    UserShellCommandFinalDelivery::Wake,
                    &completion,
                )
                .await
                .is_err()
        );
        assert_eq!(store.calls().await.append_items, writes);
        let after_retry = store
            .load_history(LoadThreadHistoryParams {
                thread_id: session.thread_id,
                include_archived: false,
            })
            .await
            .expect("retry must not alter ambiguous canonical history")
            .items;
        assert_eq!(
            serde_json::to_value(&after_retry).expect("serialize history after retry"),
            serde_json::to_value(&persisted).expect("serialize original canonical history"),
        );
    }
}
