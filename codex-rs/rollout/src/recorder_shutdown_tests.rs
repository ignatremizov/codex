use super::*;
use crate::config::RolloutConfig;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::SessionSource;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

async fn recorder(home: &TempDir) -> RolloutRecorder {
    let config = RolloutConfig {
        codex_home: home.path().to_path_buf(),
        sqlite: codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        cwd: home.path().to_path_buf(),
        model_provider_id: "test".to_string(),
        generate_memories: false,
    };
    RolloutRecorder::new(
        &config,
        RolloutRecorderParams::new(
            ThreadId::new(),
            /*forked_from_id*/ None,
            /*parent_thread_id*/ None,
            SessionSource::Exec,
            /*thread_source*/ None,
            "shutdown-test".to_string(),
            BaseInstructions::default(),
            Vec::new(),
        ),
    )
    .await
    .expect("create recorder")
}

#[tokio::test]
async fn duplicate_shutdown_waiters_observe_the_same_writer_acknowledgement() {
    let home = TempDir::new().expect("home");
    let recorder = recorder(&home).await;
    recorder.persist().await.expect("materialize rollout");
    let (first, second) = tokio::join!(recorder.shutdown(), recorder.shutdown());
    first.expect("first shutdown");
    second.expect("concurrent shutdown");
    recorder
        .shutdown()
        .await
        .expect("retry after writer task termination");
}

#[tokio::test]
async fn dropped_acknowledgement_does_not_lose_successful_writer_shutdown() {
    let home = TempDir::new().expect("home");
    let recorder = recorder(&home).await;
    recorder.persist().await.expect("materialize rollout");
    let before = tokio::fs::read(recorder.rollout_path())
        .await
        .expect("read rollout");
    let (ack, result) = oneshot::channel();
    recorder
        .tx
        .send(RolloutCmd::Shutdown { ack })
        .await
        .expect("accept writer shutdown");
    drop(result);
    recorder
        .shutdown()
        .await
        .expect("retry despite abandoned acknowledgement");
    assert_eq!(
        tokio::fs::read(recorder.rollout_path())
            .await
            .expect("read stopped rollout"),
        before
    );
}

#[tokio::test]
async fn failed_writer_shutdown_remains_retryable() {
    let home = TempDir::new().expect("home");
    let recorder = recorder(&home).await;
    let rollout_path = recorder.rollout_path();
    tokio::fs::create_dir_all(rollout_path)
        .await
        .expect("directory obstructs rollout file creation");
    recorder
        .record_canonical_items(&[RolloutItem::EventMsg(
            codex_protocol::protocol::EventMsg::AgentMessage(
                codex_protocol::protocol::AgentMessageEvent {
                    message: "buffered before shutdown".to_string(),
                    phase: None,
                    memory_citation: None,
                    delivery: None,
                    questions: None,
                },
            ),
        )])
        .await
        .expect("queue an item that shutdown must persist");
    recorder
        .persist()
        .await
        .expect_err("blocked writer cannot materialize");
    recorder
        .shutdown()
        .await
        .expect_err("failed shutdown must not acknowledge closure");
    tokio::fs::remove_dir(rollout_path)
        .await
        .expect("remove empty obstruction");
    recorder.shutdown().await.expect("retry after repair");
    recorder.shutdown().await.expect("repeated success");
}
