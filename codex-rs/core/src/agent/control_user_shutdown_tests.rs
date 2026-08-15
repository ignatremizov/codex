//! Owned user operations must drain even when their callers abandon a blocked request.

use super::AgentControlHarness;
use crate::agent::AgentStatus;
use crate::agent::control::ResumeUserInputAdmission;
use crate::agent::control::SessionPresentationId;
use crate::agent::response_observation::FinalResponseObservation;
use crate::agent::response_observation::ResponseObservationPolicy;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::turn_input::TurnInputMode;
use std::time::Duration;
use test_case::test_case;

#[derive(Clone, Copy)]
enum UserOperation {
    Prompt,
    Resume,
    ResumeWithInput,
    Observe,
    ReplaceObservation,
}

#[test_case(UserOperation::Prompt; "prompt")]
#[test_case(UserOperation::Resume; "resume")]
#[test_case(UserOperation::ResumeWithInput; "resume_with_input")]
#[test_case(UserOperation::Observe; "observe")]
#[test_case(UserOperation::ReplaceObservation; "replace_observation")]
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
                    FinalResponseObservation::Passive,
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
