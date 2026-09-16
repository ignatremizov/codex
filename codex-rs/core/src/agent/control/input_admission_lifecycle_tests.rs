use super::*;
use crate::UserAgentResponseHandling;
use crate::UserAgentSpawnOptions;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskResult;
use pretty_assertions::assert_eq;
use test_case::test_case;

struct BlockingTask {
    started: Arc<tokio::sync::Notify>,
}

impl SessionTask for BlockingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "agent_control_test.blocking"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        session
            .send_event(
                ctx.as_ref(),
                EventMsg::TurnStarted(TurnStartedEvent {
                    turn_attribution: None,
                    turn_id: ctx.sub_id.clone(),
                    root_turn_id: None,
                    trace_id: None,
                    started_at: None,
                    model_context_window: None,
                    collaboration_mode_kind: Default::default(),
                    agent_queue: None,
                }),
            )
            .await;
        self.started.notify_one();
        cancellation_token.cancelled().await;
        Ok(None)
    }
}

#[derive(Clone, Copy)]
enum Delivery {
    Immediate,
    Queued,
}

#[test_case(Delivery::Immediate; "immediate")]
#[test_case(Delivery::Queued; "queued")]
#[tokio::test]
async fn source_close_wins_while_input_waits_for_target_lifecycle(delivery: Delivery) {
    const SOURCE_TURN: &str = "admission-source";
    let harness = AgentControlHarness::new().await;
    let (_, root) = harness.start_thread().await;
    let control = root
        .session
        .services
        .local_agent_runtime
        .control(root.session.session_id());
    let sender = root
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await
        .expect("spawn sender");
    let receiver = root
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await
        .expect("spawn receiver");
    let sender = harness
        .manager
        .get_thread(sender.target_thread_id)
        .await
        .expect("sender");
    let receiver = harness
        .manager
        .get_thread(receiver.target_thread_id)
        .await
        .expect("receiver");
    let source = sender.session.presentation_id();
    let target = receiver.session.presentation_id();
    let started = Arc::new(tokio::sync::Notify::new());
    let turn = sender
        .session
        .new_turn_with_default_settings(SOURCE_TURN.to_string(), Default::default())
        .await;
    sender
        .session
        .spawn_task(
            turn,
            Vec::new(),
            BlockingTask {
                started: Arc::clone(&started),
            },
        )
        .await;
    timeout(Duration::from_secs(/*secs*/ 10), started.notified())
        .await
        .expect("sender started");
    let input = control
        .attribute_model_input(
            source,
            target.thread_id,
            SOURCE_TURN,
            /*batch_id*/ None,
            text_input("must not outlive closed source"),
        )
        .await
        .expect("capture active sender");
    let state = control.runtime.upgrade().expect("manager");
    let target_guard = state
        .acquire_live_agent_lifecycle(target.thread_id)
        .await
        .expect("target lifecycle");
    let sending_control = control.clone();
    let sending = async move {
        let policy = ResponseObservationPolicy::from_turn_parts(
            /*commentary*/ false,
            FinalResponseObservation::None,
            matches!(delivery, Delivery::Queued),
        );
        match delivery {
            Delivery::Immediate => sending_control
                .send_agent_input_observing_response(
                    target.thread_id,
                    input,
                    TurnStartOptions::default(),
                    source,
                    policy,
                )
                .await
                .map(|_| ()),
            Delivery::Queued => sending_control
                .queue_input_observing_response(QueuedInputObservationParams {
                    agent_id: target.thread_id,
                    input,
                    start_options: TurnStartOptions::default(),
                    observer: source,
                    response_observation: policy,
                    task_preview: None,
                    authored_selector: None,
                })
                .await
                .map(|_| ()),
        }
    };
    tokio::pin!(sending);
    assert!(futures::poll!(sending.as_mut()).is_pending());
    // Neither input path can pass target admission until this guard is released.
    control
        .close_agent(source.thread_id)
        .await
        .expect("close source");
    let captured_before = harness.manager.captured_ops().len();
    drop(target_guard);
    let error = timeout(Duration::from_secs(/*secs*/ 10), sending)
        .await
        .expect("submission resolves")
        .expect_err("closed source must not submit input");
    assert!(
        matches!(error.details(), CodexErrorDetails::ThreadNotFound(id) if *id == source.thread_id)
    );
    assert_eq!(harness.manager.captured_ops().len(), captured_before);
    assert_eq!(receiver.session.active_agent_response_turn_id(), None);
    assert!(!state.agent_turn_queue.has_pending(target.thread_id));
    receiver
        .shutdown_and_wait()
        .await
        .expect("shutdown receiver");
    root.shutdown_and_wait().await.expect("shutdown root");
}
