use super::RecordingThreadInstructionsProvider;
use super::assert_single_instruction_fragment;
use super::expected_provider_only_instruction_fragment;
use super::persisted_resume_history;
use super::submit_thread_turn;
use anyhow::Result;
use codex_core::StartThreadOptions;
use codex_core::UserAgentFinalResponseHandling;
use codex_core::UserAgentForkMode;
use codex_core::UserAgentResponseHandling;
use codex_core::UserAgentSpawnOptions;
use codex_features::Feature;
use codex_protocol::error::CodexErrorDetails;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use test_case::test_case;

#[test_case(false; "instruction_snapshot")]
#[test_case(true; "shared_instruction_provider")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn options_resume_reuses_surviving_child_owner_and_cold_provider(shared: bool) -> Result<()> {
    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(|config| {
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("enable V2");
            config
                .features
                .enable(Feature::Sqlite)
                .expect("enable SQLite");
        })
        .build_with_auto_env(&server)
        .await?;
    let provider = RecordingThreadInstructionsProvider::with_text("before owner recovery");
    let provider = Arc::new(if shared { provider.shared() } else { provider });
    let root = test
        .thread_manager
        .start_thread(StartThreadOptions {
            environments: Some(Vec::new()),
            thread_instructions_provider: Some(provider),
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;
    let response_handling = UserAgentResponseHandling::from_parts(
        /*commentary*/ false,
        UserAgentFinalResponseHandling::None,
        /*target_messages*/ false,
        /*queue_input*/ false,
    );
    let survivor = root.thread.spawn_agent(UserAgentSpawnOptions {
        role: None,
        model: None,
        reasoning_effort: None,
        input: None,
        fork_mode: UserAgentForkMode::None,
        response_handling,
    }).await?;
    let target = root.thread.spawn_agent(UserAgentSpawnOptions {
        role: None,
        model: None,
        reasoning_effort: None,
        input: None,
        fork_mode: UserAgentForkMode::None,
        response_handling,
    }).await?;
    let survivor_thread = test
        .thread_manager
        .get_thread(survivor.target_thread_id)
        .await?;
    let target_thread = test
        .thread_manager
        .get_thread(target.target_thread_id)
        .await?;
    let (root_id, root_history) =
        persisted_resume_history(&root.thread, test.thread_store.as_ref()).await?;
    persisted_resume_history(&survivor_thread, test.thread_store.as_ref()).await?;
    persisted_resume_history(&target_thread, test.thread_store.as_ref()).await?;
    target_thread.shutdown_and_wait().await?;
    assert!(
        test.thread_manager
            .remove_thread(&target.target_thread_id)
            .await
            .is_some()
    );
    root.thread.shutdown_and_wait().await?;
    assert!(test.thread_manager.remove_thread(&root_id).await.is_some());

    let error = test
        .thread_manager
        .ensure_multi_agent_v2_child_loaded(target.target_thread_id)
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("a surviving sibling must not replace its absent owner"))?;
    assert!(matches!(
        error.details(),
        CodexErrorDetails::InvalidRequest(message) if message.contains("resume the parent first")
    ));
    assert!(
        test.thread_manager
            .get_thread(target.target_thread_id)
            .await
            .is_err()
    );

    let cold_provider = RecordingThreadInstructionsProvider::with_text("after owner recovery");
    let cold_provider = Arc::new(if shared {
        cold_provider.shared()
    } else {
        cold_provider
    });
    let recovered = test
        .thread_manager
        .start_thread(StartThreadOptions {
            initial_history: root_history,
            environments: Some(Vec::new()),
            thread_instructions_provider: Some(cold_provider.clone()),
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;
    assert_eq!(recovered.thread_id, root_id);
    assert_eq!(cold_provider.load_count(), 1);
    // This validates the already-live child's exact control against its recovered
    // parent; creating another registry with the same session ID is not enough.
    test.thread_manager
        .ensure_multi_agent_v2_child_loaded(survivor.target_thread_id)
        .await?;
    assert!(Arc::ptr_eq(
        &test
            .thread_manager
            .get_thread(survivor.target_thread_id)
            .await?,
        &survivor_thread,
    ));
    test.thread_manager
        .ensure_multi_agent_v2_child_loaded(target.target_thread_id)
        .await?;
    let target_thread = test
        .thread_manager
        .get_thread(target.target_thread_id)
        .await?;
    let response = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("recovered-child"),
            ev_completed("recovered-child"),
        ]),
    )
    .await;
    submit_thread_turn(&target_thread, "read the recovered owner instructions").await?;
    assert_single_instruction_fragment(
        &response.single_request(),
        &expected_provider_only_instruction_fragment("after owner recovery"),
    );
    Ok(())
}
