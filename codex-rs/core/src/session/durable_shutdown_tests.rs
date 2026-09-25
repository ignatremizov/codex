use super::*;
use crate::agent::AgentStatus;
use crate::session::SubmissionAdmission;
use crate::session::command_approval::QueuedSubmission;
use crate::session::completed_session_loop_termination;
use crate::session::session_loop_termination_from_handle;
use crate::session::tests::make_session_and_context;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_thread_store::CreateThreadParams;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::InMemoryThreadStoreFailure;
use codex_thread_store::LiveThread;
use codex_thread_store::ThreadPersistenceMetadata;
use codex_thread_store::ThreadStore;
use pretty_assertions::assert_eq;
use std::future::Future;
use std::sync::Weak;
use std::task::Poll;
use std::time::Duration;
use tokio::sync::watch;

async fn running_session() -> (Arc<Session>, SessionIo) {
    let (session, _) = make_session_and_context().await;
    let session = Arc::new(session);
    let config = session.get_config().await;
    let (tx_sub, rx_sub) = async_channel::bounded::<QueuedSubmission>(4);
    let (_, rx_event) = async_channel::unbounded();
    let session_loop = tokio::spawn(handlers::submission_loop(
        Arc::clone(&session),
        config,
        rx_sub,
    ));
    let io = SessionIo {
        tx_sub,
        session: Arc::downgrade(&session),
        rx_event,
        submission_admission: Arc::clone(&session.submission_admission),
        agent_status: watch::channel(AgentStatus::PendingInit).1,
        session_loop_termination: session_loop_termination_from_handle(session_loop),
    };
    (session, io)
}

#[derive(Clone, Copy)]
enum ActorExit {
    DurableShutdown,
    WithoutAcknowledgement,
}

#[test_case::test_case(ActorExit::DurableShutdown; "acknowledged_actor_exit")]
#[test_case::test_case(ActorExit::WithoutAcknowledgement; "unacknowledged_actor_exit")]
#[tokio::test]
async fn queued_shutdown_waiters_require_durable_acknowledgement_after_actor_exit(
    actor_exit: ActorExit,
) {
    let (session, _) = make_session_and_context().await;
    let session = Arc::new(session);
    let config = session.get_config().await;
    let (tx_sub, rx_sub) = async_channel::bounded::<QueuedSubmission>(/*cap*/ 4);
    let (_, rx_event) = async_channel::unbounded();
    let (start, started) = oneshot::channel();
    let actor_session = Arc::clone(&session);
    let session_loop = tokio::spawn(async move {
        started.await.expect("release actor");
        match actor_exit {
            ActorExit::DurableShutdown => {
                handlers::submission_loop(actor_session, config, rx_sub).await;
            }
            ActorExit::WithoutAcknowledgement => drop(rx_sub),
        }
    });
    let io = SessionIo {
        tx_sub,
        session: Arc::downgrade(&session),
        rx_event,
        submission_admission: Arc::clone(&session.submission_admission),
        agent_status: watch::channel(AgentStatus::PendingInit).1,
        session_loop_termination: session_loop_termination_from_handle(session_loop),
    };
    let mut first = Box::pin(io.shutdown_durably_and_wait());
    let mut second = Box::pin(io.shutdown_durably_and_wait());
    tokio::time::timeout(
        Duration::from_secs(/*secs*/ 10),
        std::future::poll_fn(|cx| {
            assert!(first.as_mut().poll(cx).is_pending());
            assert!(second.as_mut().poll(cx).is_pending());
            if io.tx_sub.len() == 2 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }),
    )
    .await
    .expect("queue both shutdowns before starting the actor");
    // Both reply-bearing operations exist before the actor can consume either one.
    assert_eq!(io.tx_sub.len(), 2);
    start.send(()).expect("start actor");
    let (first, second) = tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
        tokio::join!(first, second)
    })
    .await
    .expect("both waiters observe actor termination despite retained queued replies");
    match actor_exit {
        ActorExit::DurableShutdown => {
            first.expect("first acknowledged waiter");
            second.expect("second acknowledged waiter");
            assert!(io.durable_shutdown_succeeded());
            assert_eq!(io.tx_sub.len(), 1);
        }
        ActorExit::WithoutAcknowledgement => {
            assert!(first.is_err());
            assert!(second.is_err());
            assert!(!io.durable_shutdown_succeeded());
            assert_eq!(io.tx_sub.len(), 2);
        }
    }
}

#[tokio::test]
async fn facade_shutdown_uses_owned_actor_after_public_forwarder_stops() {
    let (session, actor) = running_session().await;
    let actor = Arc::new(actor);
    let (tx_sub, rx_sub) = async_channel::bounded::<QueuedSubmission>(1);
    drop(rx_sub);
    let (_, rx_event) = async_channel::unbounded();
    let facade = SessionIo {
        tx_sub,
        session: Arc::downgrade(&session),
        rx_event,
        submission_admission: Arc::new(SubmissionAdmission::forwarding_to(Arc::clone(&actor))),
        agent_status: actor.agent_status.clone(),
        session_loop_termination: actor.session_loop_termination.clone(),
    };
    facade
        .shutdown_durably_and_wait()
        .await
        .expect("owned endpoint survives forwarder");
    assert!(facade.durable_shutdown_succeeded());
    assert!(
        session
            .submission_admission
            .durable_shutdown_complete
            .load(Ordering::Acquire)
    );
}

#[tokio::test]
async fn durable_shutdown_failure_retains_retry_ownership() {
    let (mut session, _) = make_session_and_context().await;
    let store = Arc::new(InMemoryThreadStore::default());
    let thread_store: Arc<dyn ThreadStore> = store.clone();
    let live_thread = LiveThread::create(
        Arc::clone(&thread_store),
        CreateThreadParams {
            session_id: session.session_id(),
            thread_id: session.thread_id,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: None,
            source: SessionSource::Exec,
            thread_source: None,
            originator: "durable_shutdown_test".to_string(),
            base_instructions: BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            runtime_workspace_roots: None,
            multi_agent_version: None,
            history_mode: Default::default(),
            subagent_history_start_ordinal: None,
            history_base: None,
            initial_window_id: uuid::Uuid::now_v7().to_string(),
            metadata: ThreadPersistenceMetadata {
                cwd: None,
                model_provider: "test".to_string(),
                memory_mode: ThreadMemoryMode::Disabled,
            },
        },
    )
    .await
    .expect("create persistence");
    session.services.thread_store = thread_store;
    session.services.live_thread = Some(live_thread);
    let (tx_event, rx_event) = async_channel::unbounded();
    session.tx_event = tx_event;
    let session = Arc::new(session);
    let config = session.get_config().await;
    let (tx_sub, rx_sub) = async_channel::bounded::<QueuedSubmission>(4);
    let session_loop = tokio::spawn(handlers::submission_loop(
        Arc::clone(&session),
        config,
        rx_sub,
    ));
    let io = SessionIo {
        tx_sub,
        session: Arc::downgrade(&session),
        rx_event,
        submission_admission: Arc::clone(&session.submission_admission),
        agent_status: watch::channel(AgentStatus::PendingInit).1,
        session_loop_termination: session_loop_termination_from_handle(session_loop),
    };
    store
        .fail_next_operation(InMemoryThreadStoreFailure::ThreadShutdown)
        .await;

    let error = tokio::time::timeout(Duration::from_secs(10), io.shutdown_durably_and_wait())
        .await
        .expect("failed shutdown replies")
        .expect_err("writer error must propagate");
    assert!(error.to_string().contains("thread shutdown"));
    assert!(
        !session
            .submission_admission
            .durable_shutdown_complete
            .load(Ordering::Acquire)
    );
    assert!(io.submit(Op::Interrupt).await.is_err());
    assert_eq!(store.calls().await.shutdown_thread, 1);
    while let Ok(event) = io.rx_event.try_recv() {
        assert!(
            !matches!(event.msg, EventMsg::ShutdownComplete),
            "failed persistence must not announce shutdown completion"
        );
    }

    store
        .fail_next_operation(InMemoryThreadStoreFailure::ThreadShutdown)
        .await;
    io.submit(Op::Shutdown)
        .await
        .expect("legacy shutdown joins durable retry");
    tokio::time::timeout(Duration::from_secs(1), async {
        while store.calls().await.shutdown_thread != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("legacy shutdown attempts the pending phase");

    // The submission loop remains alive to perform the failed phase on retry.
    tokio::time::timeout(Duration::from_secs(10), io.shutdown_durably_and_wait())
        .await
        .expect("shutdown retry completes")
        .expect("shutdown retry succeeds");
    io.shutdown_durably_and_wait()
        .await
        .expect("success is stable after session termination");
    assert_eq!(store.calls().await.shutdown_thread, 3);
}

#[tokio::test]
async fn legacy_termination_does_not_acknowledge_durable_shutdown() {
    let (tx_sub, rx_sub) = async_channel::bounded::<QueuedSubmission>(1);
    drop(rx_sub);
    let (_, rx_event) = async_channel::unbounded();
    let io = SessionIo {
        tx_sub,
        session: Weak::new(),
        rx_event,
        submission_admission: Arc::new(SubmissionAdmission::default()),
        agent_status: watch::channel(AgentStatus::PendingInit).1,
        session_loop_termination: completed_session_loop_termination(),
    };

    assert!(io.shutdown_durably_and_wait().await.is_err());
}

#[tokio::test]
async fn durable_shutdown_drains_preaccepted_delivery_before_acknowledgement() {
    let (session, io) = running_session().await;
    let admission = Arc::clone(&session.submission_admission);
    let delivery = admission
        .try_accept_completion_delivery()
        .expect("preaccepted delivery");
    let shutdown = tokio::spawn(async move { io.shutdown_durably_and_wait().await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while !admission.completion_closed.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown fences new deliveries");
    assert!(!shutdown.is_finished());
    assert!(!admission.durable_shutdown_complete.load(Ordering::Acquire));
    assert!(admission.try_accept_completion_delivery().is_none());
    // The accepted delivery must remain able to acquire send_lock while shutdown drains it.
    let send_guard = tokio::time::timeout(Duration::from_secs(1), admission.send_lock.lock())
        .await
        .expect("shutdown does not hold send lock while draining");
    drop(send_guard);
    drop(delivery);
    shutdown
        .await
        .expect("shutdown submission task")
        .expect("shutdown acknowledges after drain");
    assert!(admission.durable_shutdown_complete.load(Ordering::Acquire));
}

#[tokio::test]
async fn dropped_shutdown_reply_does_not_cancel_accepted_shutdown() {
    let (session, _) = make_session_and_context().await;
    let session = Arc::new(session);
    let config = session.get_config().await;
    let (tx_sub, rx_sub) = async_channel::bounded::<QueuedSubmission>(1);
    let (_, rx_event) = async_channel::unbounded();
    let session_loop = tokio::spawn(handlers::submission_loop(
        Arc::clone(&session),
        config,
        rx_sub,
    ));
    let io = SessionIo {
        tx_sub,
        session: Arc::downgrade(&session),
        rx_event,
        submission_admission: Arc::clone(&session.submission_admission),
        agent_status: watch::channel(AgentStatus::PendingInit).1,
        session_loop_termination: session_loop_termination_from_handle(session_loop),
    };
    let (reply, result) = oneshot::channel();
    drop(result);
    io.submit(Op::ShutdownDurably { reply })
        .await
        .expect("accept shutdown with no waiter");
    tokio::time::timeout(Duration::from_secs(10), io.shutdown_durably_and_wait())
        .await
        .expect("accepted shutdown finishes")
        .expect("second caller observes durable success");
}
