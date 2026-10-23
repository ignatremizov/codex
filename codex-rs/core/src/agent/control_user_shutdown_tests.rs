//! Owned user operations must drain even when their callers abandon a blocked request.

use super::AgentControlHarness;
use crate::agent::AgentStatus;
use crate::agent::control::AgentControlInput;
use crate::agent::control::ResumeUserInputAdmission;
use crate::agent::control::SessionPresentationId;
use crate::agent::response_observation::FinalResponseObservation;
use crate::agent::response_observation::ResponseObservationPolicy;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::turn_input::TurnInputMode;
use pretty_assertions::assert_eq;
use std::time::Duration;
use test_case::test_case;

#[derive(Clone, Copy)]
enum UserOperation {
    Prompt,
    Resume,
    ResumeWithInput,
    Observe,
    ReplaceObservation,
    Close,
    ScopedInput,
}

#[test_case(UserOperation::Prompt; "prompt")]
#[test_case(UserOperation::Resume; "resume")]
#[test_case(UserOperation::ResumeWithInput; "resume_with_input")]
#[test_case(UserOperation::Observe; "observe")]
#[test_case(UserOperation::ReplaceObservation; "replace_observation")]
#[test_case(UserOperation::Close; "close")]
#[test_case(UserOperation::ScopedInput; "scoped_input")]
#[tokio::test]
async fn cancelled_user_operation_keeps_shutdown_pending_until_lifecycle_release(
    operation: UserOperation,
) {
    let harness = AgentControlHarness::new().await;
    let state = harness.control.runtime.upgrade().expect("live manager");
    let target = ThreadId::new();
    let lifecycle = state.agent_lifecycle_lock(target).lock_owned().await;
    let observer = SessionPresentationId::new(ThreadId::new(), uuid::Uuid::new_v4());
    let policy = ResponseObservationPolicy::default();
    let mut request = Box::pin(async {
        match operation {
            UserOperation::Prompt => harness
                .control
                .send_user_input_observing_response(
                    target,
                    Vec::new(),
                    Default::default(),
                    observer,
                    policy,
                    /*task_preview*/ None,
                )
                .await
                .map(drop),
            UserOperation::Resume => harness
                .control
                .resume_user_agent_from_rollout(
                    harness.config.clone(),
                    target,
                    SessionSource::Cli,
                    policy,
                )
                .await
                .map(drop),
            UserOperation::ResumeWithInput => harness
                .control
                .resume_agent_from_rollout_with_user_input(
                    harness.config.clone(),
                    target,
                    SessionSource::Cli,
                    ResumeUserInputAdmission {
                        input: Vec::new(),
                        observer,
                        response_observation: policy,
                        admission_policy: TurnInputMode::StartIfIdle,
                        task_preview: None,
                    },
                )
                .await
                .map(drop),
            UserOperation::Observe => harness
                .control
                .ensure_durable_completion_watcher(
                    target,
                    SessionSource::Cli,
                    policy,
                    AgentStatus::NotFound,
                    /*task_preview*/ None,
                )
                .await
                .map(drop),
            UserOperation::ReplaceObservation => harness
                .control
                .replace_durable_final_response_observation(
                    target,
                    observer,
                    observer.thread_id,
                    FinalResponseObservation::Passive,
                )
                .await
                .map(drop),
            UserOperation::Close => harness
                .control
                .close_agent_with_status(target)
                .await
                .map(drop),
            UserOperation::ScopedInput => harness
                .control
                .send_agent_input_observing_response(
                    target,
                    AgentControlInput::AttributedAgentInput {
                        content: Vec::new(),
                        attribution: Box::new(codex_protocol::AgentInputAttribution {
                            sender: codex_protocol::AgentInputIdentity {
                                thread_id: observer.thread_id,
                                nickname: None,
                                agent_ref: None,
                                task_path: None,
                                role: None,
                                model: None,
                                reasoning_effort: None,
                            },
                            recipient: codex_protocol::AgentInputIdentity {
                                thread_id: target,
                                nickname: None,
                                agent_ref: None,
                                task_path: None,
                                role: None,
                                model: None,
                                reasoning_effort: None,
                            },
                            sender_turn_id: "source-turn".to_string(),
                            batch_id: None,
                        }),
                        presentation: Vec::new(),
                    },
                    Default::default(),
                    observer,
                    policy,
                )
                .await
                .map(drop),
        }
    });
    // Poll the actual admission boundary before shutting down. The owned worker cannot
    // pass the lifecycle lock, even if it has not yet been scheduled.
    assert!(futures::poll!(request.as_mut()).is_pending());
    let shutdown = harness.control.runtime.request_shutdown();
    drop(request);
    let mut drained = Box::pin(shutdown.wait());
    assert!(futures::poll!(drained.as_mut()).is_pending());
    drop(lifecycle);
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), drained)
        .await
        .expect("cancelled caller leaves a bounded owned worker")
        .expect("worker releases tree membership");
}

#[tokio::test]
async fn prepared_close_response_keeps_tree_shutdown_pending() {
    let harness = AgentControlHarness::new().await;
    let (_, root) = harness.start_thread().await;
    let control = root
        .session
        .services
        .local_agent_runtime
        .control(root.session.session_id());
    let child = root
        .spawn_agent(crate::agent::UserAgentSpawnOptions {
            fork_mode: crate::agent::UserAgentForkMode::None,
            response_handling: crate::agent::UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await
        .expect("spawn a controlled child");
    let response = control
        .prepare_close_agent_response(
            std::sync::Arc::clone(&root.session),
            codex_protocol::protocol::MultiAgentVersion::V1,
            child.target_thread_id,
        )
        .await
        .expect("prepare an owned close response");
    let shutdown = control.runtime.request_shutdown();
    let mut drained = Box::pin(shutdown.wait());
    assert!(futures::poll!(drained.as_mut()).is_pending());
    drop(response);
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), drained)
        .await
        .expect("close response ownership must release membership")
        .expect("tree shutdown must drain");
    harness
        .manager
        .get_thread(child.target_thread_id)
        .await
        .expect("live child")
        .shutdown_and_wait()
        .await
        .expect("stop child");
    root.shutdown_and_wait().await.expect("stop root");
}

#[tokio::test]
async fn tree_shutdown_cancels_pending_queue_without_waiting_for_target_completion() {
    use crate::agent::UserAgentForkMode;
    use crate::agent::UserAgentResponseHandling;
    use crate::agent::control::QueuedInputObservationParams;

    let harness = AgentControlHarness::new().await;
    let (_, root) = harness.start_thread().await;
    let control = root
        .session
        .services
        .local_agent_runtime
        .control(root.session.session_id());
    let child = root
        .spawn_agent(crate::agent::UserAgentSpawnOptions {
            fork_mode: UserAgentForkMode::None,
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await
        .expect("spawn an idle controlled child");
    let child = harness
        .manager
        .get_thread(child.target_thread_id)
        .await
        .expect("live child");
    assert!(child.session.begin_agent_response_turn("busy-target"));
    let accepted = control
        .queue_input_observing_response(QueuedInputObservationParams {
            agent_id: child.session.thread_id(),
            input: AgentControlInput::User(vec![codex_protocol::user_input::UserInput::Text {
                text: "pending work must not delay shutdown".to_string(),
                text_elements: Vec::new(),
            }]),
            start_options: Default::default(),
            observer: root.session.presentation_id(),
            response_observation: ResponseObservationPolicy::from_turn_parts(
                /*commentary*/ false,
                FinalResponseObservation::Passive,
                /*queue_input*/ true,
            ),
            task_preview: None,
            authored_selector: Some(child.session.thread_id().to_string()),
        })
        .await
        .expect("accept a pending turn");
    assert_eq!(
        control
            .list_queued_agent_turns()
            .into_iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec![accepted.queue_id]
    );
    let shutdown = control.runtime.request_shutdown();
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), shutdown.wait())
        .await
        .expect("queue worker must wake on tree shutdown")
        .expect("owned queue work must drain");
    assert!(control.list_queued_agent_turns().is_empty());
    assert_eq!(
        child.session.active_agent_response_turn_id().as_deref(),
        None
    );
    child.shutdown_and_wait().await.expect("stop child");
    root.shutdown_and_wait().await.expect("stop root");
}
