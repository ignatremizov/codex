use super::*;
use crate::session::Submission;
use futures::FutureExt;
use pretty_assertions::assert_eq;
use std::sync::atomic::Ordering;
use std::time::Duration;

fn submission(id: &str, op: Op) -> Submission {
    Submission {
        id: id.to_string(),
        op,
        trace: None,
        parent_turn_id: None,
        root_turn_id: None,
        residency_guard: None,
    }
}

fn shell_submission(id: &str) -> Submission {
    submission(
        id,
        Op::RunUserShellCommand {
            command: "never executed by this admission test".to_string(),
            timeout_ms: None,
            response_handling: Default::default(),
        },
    )
}

#[tokio::test]
async fn accepted_shell_envelope_owns_its_exact_session_before_a_queued_shutdown() {
    for asynchronous in [false, true] {
        let admission = Arc::new(SubmissionAdmission::default());
        let (sender, receiver) = async_channel::bounded(/*cap*/ 2);
        if asynchronous {
            admission
                .enqueue(&sender, shell_submission("shell"))
                .await
                .expect("accept shell");
        } else {
            admission
                .try_enqueue(&sender, shell_submission("shell"))
                .expect("accept shell");
        }
        admission
            .enqueue(&sender, submission("shutdown", Op::Shutdown))
            .await
            .expect("accept shutdown behind shell");
        let mut shell = receiver.recv().await.expect("accepted shell envelope");
        assert_eq!(shell.submission.id, "shell");
        let completion = shell
            .user_shell_completion
            .take()
            .expect("captured before dequeue");
        assert!(admission.try_accept_completion_delivery().is_none());
        assert!(
            admission
                .try_enqueue(&sender, shell_submission("late"))
                .is_err()
        );
        assert!(admission.admit_injection().await.is_err());
        drop(
            admission
                .admit_completion(&completion)
                .await
                .expect("accepted shell can finish"),
        );
        let foreign = Arc::new(SubmissionAdmission::default());
        assert!(foreign.admit_completion(&completion).await.is_err());
        let mut drain = Box::pin(admission.drain_accepted_completions());
        assert!(drain.as_mut().now_or_never().is_none());
        drop(completion);
        tokio::time::timeout(Duration::from_secs(/*secs*/ 5), drain)
            .await
            .expect("dropping the exact shell receipt unblocks shutdown");
        assert_eq!(admission.accepted_completions.load(Ordering::Acquire), 0);
        let shutdown = receiver.recv().await.expect("shutdown envelope");
        assert_eq!(shutdown.submission.id, "shutdown");
        assert!(shutdown.user_shell_completion.is_none());
    }
}

#[tokio::test]
async fn failed_or_cancelled_shell_enqueue_releases_only_its_unaccepted_receipt() {
    let admission = Arc::new(SubmissionAdmission::default());
    let (sender, receiver) = async_channel::bounded(/*cap*/ 1);
    admission
        .try_enqueue(&sender, submission("occupied", Op::Compact))
        .expect("fill queue");
    assert!(
        admission
            .try_enqueue(&sender, shell_submission("rejected"))
            .is_err()
    );
    assert_eq!(admission.accepted_completions.load(Ordering::Acquire), 0);
    let mut blocked = Box::pin(admission.enqueue(&sender, shell_submission("cancelled")));
    assert!(blocked.as_mut().now_or_never().is_none());
    assert_eq!(admission.accepted_completions.load(Ordering::Acquire), 1);
    drop(blocked);
    assert_eq!(admission.accepted_completions.load(Ordering::Acquire), 0);
    assert_eq!(
        receiver
            .recv()
            .await
            .expect("original queue member")
            .submission
            .id,
        "occupied"
    );
    assert!(receiver.try_recv().is_err());
    drop(receiver);
    assert!(
        admission
            .enqueue(&sender, shell_submission("closed"))
            .await
            .is_err()
    );
    assert_eq!(admission.accepted_completions.load(Ordering::Acquire), 0);
}
