use super::*;
use crate::session::SessionIo;
use codex_protocol::protocol::AgentStatus;
use futures::FutureExt;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::time::timeout;

async fn request(cancellation: CancellationToken) -> DecoderRequest {
    let (parent, turn, _events) = crate::session::tests::make_session_and_context_with_rx().await;
    DecoderRequest {
        config: turn.config.as_ref().clone(),
        parent,
        turn,
        history: InitialHistory::New,
        cancellation,
        deadline: Instant::now() + Duration::from_secs(/*secs*/ 30),
    }
}

#[tokio::test]
async fn cancelled_start_does_not_poll_initialization() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let request = request(cancellation).await;
    let startup = Arc::new(SessionStartup::default());
    let result = timeout(
        Duration::from_secs(/*secs*/ 5),
        run_with_startup(request, Arc::clone(&startup)),
    )
    .await
    .expect("cancelled startup finishes");
    assert!(result.is_err());
    assert!(startup.session.get().is_none());
    assert!(startup.io.get().is_none());
}

#[tokio::test]
async fn expired_deadline_does_not_poll_initialization() {
    let mut request = request(CancellationToken::new()).await;
    request.deadline = Instant::now() - Duration::from_secs(/*secs*/ 1);
    let startup = Arc::new(SessionStartup::default());
    let result = timeout(
        Duration::from_secs(/*secs*/ 5),
        run_with_startup(request, Arc::clone(&startup)),
    )
    .await
    .expect("expired startup finishes");
    assert!(result.is_err());
    assert!(startup.session.get().is_none());
}

#[tokio::test]
async fn dropped_caller_keeps_cleanup_owned_until_actor_termination() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let request = request(cancellation).await;
    let startup = Arc::new(SessionStartup::default());
    let weak_startup = Arc::downgrade(&startup);
    let mut config = request.config.clone();
    config.ephemeral = true;
    let manager =
        crate::thread_manager::ThreadManager::with_models_provider_home_and_state_for_tests(
            codex_login::CodexAuth::from_api_key("dummy"),
            config.model_provider.clone(),
            config.codex_home.to_path_buf(),
            Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
            /*state_db*/ None,
        );
    let thread = manager
        .start_thread(crate::thread_manager::StartThreadOptions::new(config))
        .await
        .expect("start decoder actor fixture")
        .thread;
    let mut io = thread.io.clone();
    let actor_termination = io.session_loop_termination.clone();
    let (terminated, termination) = oneshot::channel();
    io.session_loop_termination = async {
        actor_termination.await;
        let _ = termination.await;
    }
    .boxed()
    .shared();
    startup.set_session(Arc::clone(&thread.session));
    assert!(startup.io.set(io).is_ok());
    let caller = tokio::spawn(run_with_startup(request, startup));
    timeout(Duration::from_secs(/*secs*/ 5), async {
        while !thread.io.durable_shutdown_succeeded() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cleanup receives the actor's durable shutdown acknowledgement");
    assert!(
        !caller.is_finished(),
        "durable acknowledgement is not the termination barrier"
    );
    caller.abort();
    let _ = caller.await;
    assert!(
        weak_startup.upgrade().is_some(),
        "worker still owns cleanup"
    );
    terminated.send(()).expect("termination observer");
    timeout(Duration::from_secs(/*secs*/ 5), async {
        while weak_startup.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cleanup releases startup after termination");
}

#[tokio::test]
async fn closed_shutdown_channel_still_waits_for_actual_termination() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let request = request(cancellation).await;
    let startup = Arc::new(SessionStartup::default());
    let (tx_sub, rx_sub) = async_channel::bounded(/*cap*/ 1);
    drop(rx_sub);
    let (_events, rx_event) = async_channel::unbounded();
    let (_status, agent_status) = watch::channel(AgentStatus::Running);
    let (terminated, termination) = oneshot::channel();
    let entered = Arc::new(tokio::sync::Notify::new());
    let observed = Arc::clone(&entered);
    assert!(
        startup
            .io
            .set(SessionIo {
                tx_sub,
                submission_admission: Arc::new(crate::session::SubmissionAdmission::default()),
                rx_event,
                agent_status,
                session_loop_termination: async move {
                    observed.notify_one();
                    let _ = termination.await;
                }
                .boxed()
                .shared(),
            })
            .is_ok()
    );
    let caller = tokio::spawn(run_with_startup(request, startup));
    timeout(Duration::from_secs(/*secs*/ 5), entered.notified())
        .await
        .expect("termination wait entered");
    assert!(!caller.is_finished());
    terminated.send(()).expect("termination observer");
    assert!(
        timeout(Duration::from_secs(/*secs*/ 5), caller)
            .await
            .expect("cleanup finishes")
            .expect("worker joins")
            .is_err()
    );
}

#[tokio::test]
async fn panicked_lifetime_owner_is_not_an_ordinary_retryable_decoder_error() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let request = request(cancellation).await;
    let startup = Arc::new(SessionStartup::default());
    let (tx_sub, rx_sub) = async_channel::bounded(/*cap*/ 1);
    drop(rx_sub);
    let (_events, rx_event) = async_channel::unbounded();
    let (_status, agent_status) = watch::channel(AgentStatus::Running);
    assert!(
        startup
            .io
            .set(SessionIo {
                tx_sub,
                submission_admission: Arc::new(crate::session::SubmissionAdmission::default()),
                rx_event,
                agent_status,
                session_loop_termination: async {
                    panic!("lost cleanup owner");
                }
                .boxed()
                .shared(),
            })
            .is_ok()
    );
    let error = timeout(
        Duration::from_secs(/*secs*/ 5),
        run_with_startup(request, startup),
    )
    .await
    .expect("panicked owner joins")
    .expect_err("lost lifecycle owner");
    assert!(error.is::<DecoderWorkerLost>());
}
