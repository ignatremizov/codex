//! Successful completion/permission rollback scenarios exercise Legacy history.
//! Paginated history rejects that API without mutating or quarantining the session.

use anyhow::Result;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_features::Feature;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_thread_store::LoadThreadHistoryParams;
use core_test_support::ThreadIdle;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;
use test_case::test_case;

#[test_case(MultiAgentVersion::V1; "v1")]
#[test_case(MultiAgentVersion::V2; "v2")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paginated_rollback_rejection_preserves_history_and_next_turn(
    version: MultiAgentVersion,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let first = responses::mount_sse_once(&server, responses::sse_completed("before")).await;
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let test = test_codex()
        .with_history_mode(ThreadHistoryMode::Paginated)
        .with_extensions(Arc::new(extensions.build()))
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should enable agent controls");
            match version {
                MultiAgentVersion::V1 => config
                    .features
                    .disable(Feature::MultiAgentV2)
                    .expect("V1 fixture should disable V2"),
                MultiAgentVersion::V2 => config
                    .features
                    .enable(Feature::MultiAgentV2)
                    .expect("V2 fixture should enable V2"),
                MultiAgentVersion::Disabled => unreachable!("multi-agent fixture"),
            };
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_text_turn("Keep this instruction after rejected rollback.")
        .await?;
    ThreadIdle::wait(&test.codex).await;
    first.single_request();
    test.codex.flush_rollout().await?;
    let history_params = LoadThreadHistoryParams {
        thread_id: test.session_configured.thread_id,
        include_archived: false,
    };
    let before = test
        .thread_store
        .load_canonical_artifact_segments(history_params.clone())
        .await?;
    let rollback_id = test
        .codex
        .submit(Op::ThreadRollback { num_turns: 1 })
        .await?;
    let error = tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
        loop {
            let event = test.codex.next_event().await?;
            if event.id != rollback_id {
                continue;
            }
            match event.msg {
                EventMsg::Error(error) => break Ok::<_, anyhow::Error>(error),
                EventMsg::ThreadRolledBack(_) => {
                    anyhow::bail!("Paginated history must not acknowledge Legacy rollback");
                }
                _ => {}
            }
        }
    })
    .await??;
    assert_eq!(
        error.codex_error_info,
        Some(CodexErrorInfo::ThreadRollbackFailed)
    );
    assert!(
        error
            .message
            .contains("thread/rollback only supports Legacy history"),
        "{}",
        error.message
    );
    test.codex.flush_rollout().await?;
    let after = test
        .thread_store
        .load_canonical_artifact_segments(history_params)
        .await?;
    assert_eq!(
        serde_json::to_value(after.segments)?,
        serde_json::to_value(before.segments)?
    );

    let next = responses::mount_sse_once(&server, responses::sse_completed("after")).await;
    test.submit_text_turn("Continue after the rejected rollback.")
        .await?;
    assert!(
        next.single_request()
            .body_contains_text("Keep this instruction after rejected rollback.")
    );
    Ok(())
}
