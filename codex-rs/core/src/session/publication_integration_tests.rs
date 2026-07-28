//! Rebase seams: direct settings replies, canonical source revisions, and delivery fences.

use super::*;
use crate::session::prepared_history_items::PreparedHistoryItems;
use crate::session::thread_settings;
use codex_history::CodexHarnessMetadata;
use codex_history::RetainedContextEntry;
use codex_history::RetainedUserMessage;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsAppliedEvent;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;

async fn wait_until_gated(store: &GatedAppendStore) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while store.gate_polls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("canonical writer reached its gate");
}

#[tokio::test]
async fn standalone_settings_reply_follows_publication_but_precedes_blocked_live_events() {
    for phase in [
        AppendGate::BeforeCommit,
        AppendGate::BeforeFlush,
        AppendGate::AmbiguousFailure,
        AppendGate::FlushFailure,
    ] {
        let (mut session, store, release) = gated_session(phase).await;
        let snapshot = session.thread_settings_snapshot().await;
        let (events, event_receiver) = async_channel::bounded(1);
        events
            .send(Event {
                id: "held-by-caller".to_string(),
                msg: EventMsg::ThreadSettingsApplied(ThreadSettingsAppliedEvent {
                    thread_id: Some(session.thread_id()),
                    thread_settings: snapshot.clone(),
                }),
            })
            .await
            .expect("fill the caller's event queue");
        Arc::get_mut(&mut session).expect("unique fixture").tx_event = events;
        let (reply, mut response) = oneshot::channel();
        let mut updating = Box::pin(thread_settings::update(
            &session,
            "settings-publication".to_string(),
            ThreadSettingsOverrides::default(),
            Some(reply),
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            while store.gate_polls.load(Ordering::SeqCst) == 0 {
                assert!(futures::poll!(updating.as_mut()).is_pending());
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("standalone settings dispatched their publication");
        assert!(matches!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        // Only the result waiter is cancelled. The worker must still answer the RPC.
        drop(updating);
        release.send(()).expect("release the accepted publication");
        let result = tokio::time::timeout(Duration::from_secs(5), response)
            .await
            .expect("reply cannot wait for the blocked event consumer")
            .expect("the worker owns the direct reply");
        let succeeds = matches!(phase, AppendGate::BeforeCommit | AppendGate::BeforeFlush);
        if succeeds {
            result.expect("published settings acknowledged");
        } else {
            assert!(matches!(
                result
                    .expect_err("publication failure must reject the RPC")
                    .details(),
                CodexErrorDetails::Fatal(_)
            ));
        }
        assert_eq!(
            event_receiver.try_recv().expect("held event").id,
            "held-by-caller"
        );
        if succeeds {
            let event = tokio::time::timeout(Duration::from_secs(5), event_receiver.recv())
                .await
                .expect("event follows the direct reply")
                .expect("settings event");
            assert_eq!(event.id, "settings-publication");
            let EventMsg::ThreadSettingsApplied(applied) = event.msg else {
                panic!("expected settings acknowledgement");
            };
            assert_eq!(applied.thread_settings, snapshot);
        }
        session.await_history_publication().await;
        assert!(event_receiver.try_recv().is_err());
        assert_eq!(session.check_history_publication().is_ok(), succeeds);
        assert_eq!(session.submission_admission.requires_reload(), !succeeds);
        assert_eq!(session.submission_admission.check_ready().is_ok(), succeeds);
        assert_eq!(store.appends.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn canonical_source_revisions_survive_publication_without_rewinding_input_reservations() {
    let (session, store, release) = gated_session(AppendGate::BeforeFlush).await;
    let mut user = session.response_item_from_user_input(vec![UserInput::Text {
        text: "Only inspect the repository.".to_string(),
        text_elements: Vec::new(),
    }]);
    Session::assign_missing_response_item_id(&mut user);
    let permit = thread_settings::acquire_persistence_lock(&session).await;
    let prepared = {
        let mut state = session.state.lock().await;
        let order = state.history.reserve_input_order();
        PreparedHistoryItems::new(
            &state.history,
            vec![
                render_inventory("publication", &[]),
                ResponseItemEnvelope {
                    item: user,
                    metadata: Some(CodexHarnessMetadata {
                        user_input_order: Some(order),
                        ..Default::default()
                    }),
                },
            ],
            TruncationPolicy::Tokens(1000),
        )
    };
    let canonical = prepared.rollout_items();
    let expected = canonical
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(envelope) => Some(envelope.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        expected[1]
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.retained_source.as_ref())
            .is_some()
    );
    let receiver = session
        .dispatch_history_publication(
            permit,
            canonical,
            Vec::new(),
            /*acknowledgement*/ None,
            move |state| prepared.install(&mut state.history),
        )
        .expect("dispatch prepared originals");
    wait_until_gated(&store).await;
    assert!(session.clone_history().await.annotated_items().is_empty());
    let later_order = session.state.lock().await.history.reserve_input_order();
    drop(receiver);
    release.send(()).expect("release writer flush");
    session.await_history_publication().await;
    assert_eq!(session.clone_history().await.annotated_items(), expected);
    assert!(session.state.lock().await.history.reserve_input_order() > later_order);
    let stored = store
        .inner
        .load_history(LoadThreadHistoryParams {
            thread_id: session.thread_id(),
            include_archived: false,
        })
        .await
        .expect("canonical stored history");
    let persisted = stored
        .items
        .into_iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(item) => Some(item),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(persisted, expected);
}

#[tokio::test]
async fn confirmed_delivery_publishes_after_flush_without_retaining_session() {
    let (session, store, release) = gated_session(AppendGate::BeforeFlush).await;
    let weak = Arc::downgrade(&session);
    let state = Arc::clone(&session.state);
    let message = RetainedUserMessage {
        origin: codex_history::UserInputOrigin::User,
        turn_id: "delivery-turn".to_string(),
        message_id: Some("confirmed-send".to_string()),
        text: "May I publish?".to_string(),
        complete: true,
        phase: None,
    };
    let (task, mut completion) = session.record_delivered_assistant_message(message.clone());
    wait_until_gated(&store).await;
    assert!(
        state
            .lock()
            .await
            .history
            .retained_context()
            .ordered_entries()
            .next()
            .is_none()
    );
    assert!(
        !completion
            .has_changed()
            .expect("delivery is not yet published")
    );
    drop(session);
    assert!(weak.upgrade().is_none());
    release
        .send(())
        .expect("receipt worker owns the accepted write");
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("receipt completes without its Session")
        .expect("receipt worker");
    assert!(completion.changed().await.is_err());
    let messages = state
        .lock()
        .await
        .history
        .retained_context()
        .ordered_entries()
        .filter_map(|(_, entry)| match entry {
            RetainedContextEntry::AssistantMessage(message) => Some(message.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(messages, vec![message]);
    assert_eq!(store.appends.load(Ordering::SeqCst), 1);
}
