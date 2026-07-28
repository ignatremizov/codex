//! Cancellation and goal-first contention against a real LiveThread publication boundary.
use super::*;
use codex_extension_api::PostCompactionContextContribution;
use codex_extension_api::TurnInputContribution;
use codex_utils_output_truncation::TruncationPolicy;
use tokio::sync::Semaphore;
use tokio::sync::oneshot;

#[tokio::test]
async fn accepted_transcript_publication_enqueues_after_flush_even_without_receipt_waiter() {
    for phase in [
        AppendGate::BeforeCommit,
        AppendGate::AfterCommit,
        AppendGate::AmbiguousFailure,
        AppendGate::BeforeFlush,
        AppendGate::FlushFailure,
    ] {
        let (mut session, store, release) = gated_session(phase).await;
        let (sender, events) = async_channel::unbounded();
        Arc::get_mut(&mut session)
            .expect("no accepted worker yet")
            .tx_event = sender;
        let turn = session.new_default_turn().await;
        let item = codex_protocol::models::ResponseItem::AgentMessage {
            id: Some(codex_protocol::ResponseItemId::with_suffix(
                "amsg", "accepted",
            )),
            author: "/root".into(),
            recipient: "/root/worker".into(),
            content: vec![
                codex_protocol::models::AgentMessageInputContent::InputText {
                    text: "accepted transcript".into(),
                },
            ],
            internal_chat_message_metadata_passthrough: None,
        };
        let batch = session
            .conversation_publication_batch(
                &turn,
                vec![RolloutItem::ResponseItem(item.clone().into())],
                std::slice::from_ref(&item),
            )
            .await;
        let expected = batch.events.clone();
        let installed = Arc::new(AtomicUsize::new(0));
        let install = Arc::clone(&installed);
        let permit = session
            .acquire_history_publication_barrier()
            .await
            .expect("barrier");
        let receipt = session
            .dispatch_history_publication_with_events(
                permit,
                batch,
                Vec::new(),
                /*acknowledgement*/ None,
                move |_| {
                    install.fetch_add(1, Ordering::SeqCst);
                },
            )
            .expect("accepted");
        tokio::time::timeout(Duration::from_secs(5), async {
            while store.gate_polls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker reached gate");
        assert!(
            events.try_recv().is_err(),
            "no event before canonical flush"
        );
        assert_eq!(installed.load(Ordering::SeqCst), 0);
        drop(receipt);
        release.send(()).expect("worker retained ownership");
        tokio::time::timeout(Duration::from_secs(5), session.await_history_publication())
            .await
            .expect("accepted publication finishes without a waiter");
        let succeeded = !matches!(
            phase,
            AppendGate::AmbiguousFailure | AppendGate::FlushFailure
        );
        assert_eq!(installed.load(Ordering::SeqCst), usize::from(succeeded));
        assert_eq!(store.appends.load(Ordering::SeqCst), 1);
        assert_eq!(session.check_history_publication().is_ok(), succeeded);
        if !succeeded {
            assert!(session.acquire_history_publication_barrier().await.is_err());
            assert!(
                session
                    .inject_client_response_items(vec![item], &turn)
                    .await
                    .is_err()
            );
            assert_eq!(store.appends.load(Ordering::SeqCst), 1);
        }
        let actual = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(
            serde_json::to_value(actual).expect("events"),
            serde_json::to_value(if succeeded { expected } else { Vec::new() }).expect("expected"),
        );
    }
}

#[tokio::test]
async fn transcript_publication_receipt_does_not_require_a_connected_event_reader() {
    let (mut session, store, release) = gated_session(AppendGate::BeforeCommit).await;
    let (sender, events) = async_channel::unbounded();
    drop(events);
    Arc::get_mut(&mut session).expect("no worker").tx_event = sender;
    let turn = session.new_default_turn().await;
    let item = render_inventory("closed reader", &[]);
    let batch = session
        .conversation_publication_batch(
            &turn,
            vec![RolloutItem::ResponseItem(item.clone())],
            std::slice::from_ref(&item.item),
        )
        .await;
    let permit = session
        .acquire_history_publication_barrier()
        .await
        .expect("barrier");
    let receipt = session
        .dispatch_history_publication_with_events(
            permit,
            batch,
            Vec::new(),
            /*acknowledgement*/ None,
            |_| (),
        )
        .expect("accepted");
    release.send(()).expect("release append");
    tokio::time::timeout(Duration::from_secs(5), session.publication_result(receipt))
        .await
        .expect("publication finishes")
        .expect("canonical success");
    assert_eq!(store.appends.load(Ordering::SeqCst), 1);
    assert!(session.check_history_publication().is_ok());
}

async fn gated_session(
    phase: AppendGate,
) -> (Arc<Session>, Arc<GatedAppendStore>, oneshot::Sender<()>) {
    let (mut session, _) = make_session_and_context().await;
    let (release, gate) = oneshot::channel();
    let store = Arc::new(GatedAppendStore {
        inner: InMemoryThreadStore::default(),
        phase,
        release: AsyncMutex::new(Some(gate)),
        appends: AtomicUsize::new(0),
        gate_polls: AtomicUsize::new(0),
        writer: AsyncMutex::new(()),
    });
    let config = session.get_config().await;
    session.services.live_thread = Some(
        LiveThread::create(
            store.clone(),
            CreateThreadParams {
                session_id: session.session_id(),
                thread_id: session.thread_id,
                extra_config: None,
                forked_from_id: None,
                parent_thread_id: None,
                source: SessionSource::Exec,
                thread_source: None,
                originator: "publication-test".to_string(),
                base_instructions: BaseInstructions::default(),
                dynamic_tools: Vec::new(),
                selected_capability_roots: Vec::new(),
                multi_agent_version: None,
                history_mode: ThreadHistoryMode::Legacy,
                subagent_history_start_ordinal: None,
                history_base: None,
                initial_window_id: uuid::Uuid::now_v7().to_string(),
                runtime_workspace_roots: None,
                metadata: ThreadPersistenceMetadata {
                    cwd: Some(config.cwd.to_path_buf()),
                    model_provider: config.model_provider_id.clone(),
                    memory_mode: ThreadMemoryMode::Disabled,
                },
            },
        )
        .await
        .expect("create thread"),
    );
    (Arc::new(session), store, release)
}

#[tokio::test]
async fn cancelled_checkpoint_waiter_cannot_strand_goal_lease_or_reappend_committed_batch() {
    for phase in [
        AppendGate::BeforeCommit,
        AppendGate::AfterCommit,
        AppendGate::AmbiguousFailure,
        AppendGate::BeforeFlush,
        AppendGate::FlushFailure,
    ] {
        let (session, store, release) = gated_session(phase).await;
        let goal = Arc::new(Semaphore::new(/*permits*/ 1));
        let lease = goal.clone().acquire_owned().await.expect("goal lease");
        let permit = crate::session::thread_settings::acquire_persistence_lock(&session).await;
        let count = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&count);
        let (_, acknowledgement) =
            TurnInputContribution::with_acknowledgement(Vec::new(), move || {
                observed.fetch_add(1, Ordering::SeqCst);
            })
            .into_parts();
        let envelope = render_inventory("publication", &[]);
        let expected = envelope.clone();
        let receiver = session
            .dispatch_history_publication(
                permit,
                vec![RolloutItem::ResponseItem(envelope.clone())],
                vec![
                    PostCompactionContextContribution::with_lease_and_validation(
                        Vec::new(),
                        lease,
                        || true,
                    ),
                ],
                acknowledgement,
                move |state| {
                    state
                        .history
                        .record_annotated_items(&[envelope], TruncationPolicy::Tokens(1000))
                },
            )
            .expect("dispatch");
        tokio::time::timeout(Duration::from_secs(5), async {
            while store.gate_polls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("append reached gate");
        drop(receiver);
        let mut second_goal = Box::pin(goal.acquire());
        assert!(futures::poll!(second_goal.as_mut()).is_pending());
        release.send(()).expect("worker owns append");
        // No explicit drain or subsequent history write drives the predecessor.
        let _second_goal = tokio::time::timeout(Duration::from_secs(5), second_goal)
            .await
            .expect("goal mutation unblocked")
            .expect("goal semaphore");
        assert_eq!(store.appends.load(Ordering::SeqCst), 1);
        let succeeded = !matches!(
            phase,
            AppendGate::AmbiguousFailure | AppendGate::FlushFailure
        );
        assert_eq!(count.load(Ordering::SeqCst), usize::from(succeeded));
        assert_eq!(
            session.clone_history().await.annotated_items(),
            if succeeded {
                std::slice::from_ref(&expected)
            } else {
                &[]
            }
        );
        assert_eq!(session.check_history_publication().is_ok(), succeeded);
        if !succeeded {
            let permit = crate::session::thread_settings::acquire_persistence_lock(&session).await;
            assert!(
                session
                    .dispatch_history_publication(
                        permit,
                        Vec::new(),
                        Vec::new(),
                        /*acknowledgement*/ None,
                        |_| ()
                    )
                    .is_err()
            );
            assert_eq!(store.appends.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test]
async fn cancellation_before_dispatch_does_not_append_or_acknowledge() {
    let (session, store, _release) = gated_session(AppendGate::BeforeCommit).await;
    let turn = session.new_default_turn().await;
    let permit = crate::session::thread_settings::acquire_persistence_lock(&session).await;
    let count = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&count);
    let (_, acknowledgement) = TurnInputContribution::with_acknowledgement(Vec::new(), move || {
        observed.fetch_add(1, Ordering::SeqCst);
    })
    .into_parts();
    let mut publication = Box::pin(session.record_durable_context_items(
        &turn,
        turn.model_info(),
        vec![render_inventory("publication", &[]).item],
        acknowledgement,
    ));
    assert!(futures::poll!(publication.as_mut()).is_pending());
    drop(publication);
    drop(permit);
    session.await_history_publication().await;
    assert_eq!(store.appends.load(Ordering::SeqCst), 0);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(session.clone_history().await.annotated_items().is_empty());
}

#[tokio::test]
async fn closing_drains_accepted_work_and_rejects_later_publication() {
    let (session, store, release) = gated_session(AppendGate::BeforeFlush).await;
    let permit = crate::session::thread_settings::acquire_persistence_lock(&session).await;
    let receiver = session
        .dispatch_history_publication(
            permit,
            vec![RolloutItem::ResponseItem(render_inventory(
                "publication",
                &[],
            ))],
            Vec::new(),
            /*acknowledgement*/ None,
            |_| (),
        )
        .expect("dispatch");
    tokio::time::timeout(Duration::from_secs(5), async {
        while store.gate_polls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("flush reached gate");
    let mut closing = Box::pin(session.close_history_publication());
    assert!(futures::poll!(closing.as_mut()).is_pending());
    release.send(()).expect("release flush");
    tokio::time::timeout(Duration::from_secs(5), closing)
        .await
        .expect("close drains worker");
    assert!(receiver.await.expect("completion").is_ok());
    let permit = crate::session::thread_settings::acquire_persistence_lock(&session).await;
    assert!(
        session
            .dispatch_history_publication(
                permit,
                Vec::new(),
                Vec::new(),
                /*acknowledgement*/ None,
                |_| (),
            )
            .is_err()
    );
    assert_eq!(store.appends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn panicked_worker_leaves_sticky_failure_and_never_acknowledges() {
    let (session, store, release) = gated_session(AppendGate::BeforeCommit).await;
    let permit = crate::session::thread_settings::acquire_persistence_lock(&session).await;
    let count = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&count);
    let (_, acknowledgement) = TurnInputContribution::with_acknowledgement(Vec::new(), move || {
        observed.fetch_add(1, Ordering::SeqCst);
    })
    .into_parts();
    let receiver = session
        .dispatch_history_publication(
            permit,
            vec![RolloutItem::ResponseItem(render_inventory(
                "publication",
                &[],
            ))],
            Vec::new(),
            acknowledgement,
            |_| -> () { panic!("installation abandoned") },
        )
        .expect("dispatch");
    release.send(()).expect("release append");
    assert!(
        tokio::time::timeout(Duration::from_secs(5), session.publication_result(receiver))
            .await
            .expect("lost completion observed")
            .is_err()
    );
    assert!(session.check_history_publication().is_err());
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(store.appends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn invalidated_goal_lease_is_rejected_before_append() {
    let (session, store, _release) = gated_session(AppendGate::BeforeCommit).await;
    let goal = Arc::new(codex_extension_api::ActiveGoalObjective::default());
    let generation = goal.set_enabled(true).expect("enable");
    let validation_goal = Arc::clone(&goal);
    let contribution =
        PostCompactionContextContribution::with_lease_and_validation(Vec::new(), (), move || {
            validation_goal.enabled() && validation_goal.generation() == generation
        });
    goal.set_enabled(false).expect("disable");
    let permit = crate::session::thread_settings::acquire_persistence_lock(&session).await;
    assert!(
        session
            .dispatch_history_publication(
                permit,
                vec![RolloutItem::ResponseItem(render_inventory(
                    "publication",
                    &[]
                ))],
                vec![contribution],
                /*acknowledgement*/ None,
                |_| (),
            )
            .is_err()
    );
    assert_eq!(store.appends.load(Ordering::SeqCst), 0);
    assert!(session.clone_history().await.annotated_items().is_empty());
}

#[tokio::test]
async fn accepted_worker_does_not_keep_session_alive() {
    let (session, store, release) = gated_session(AppendGate::BeforeCommit).await;
    let weak = Arc::downgrade(&session);
    let permit = crate::session::thread_settings::acquire_persistence_lock(&session).await;
    let receiver = session
        .dispatch_history_publication(
            permit,
            vec![RolloutItem::ResponseItem(render_inventory(
                "publication",
                &[],
            ))],
            Vec::new(),
            /*acknowledgement*/ None,
            |_| (),
        )
        .expect("dispatch");
    tokio::time::timeout(Duration::from_secs(5), async {
        while store.gate_polls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("append reached gate");
    drop(session);
    assert!(weak.upgrade().is_none());
    release.send(()).expect("worker alive without session");
    tokio::time::timeout(Duration::from_secs(5), receiver)
        .await
        .expect("worker finishes")
        .expect("result channel")
        .expect("publication");
}
