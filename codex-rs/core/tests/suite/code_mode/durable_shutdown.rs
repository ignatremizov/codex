use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::config::Config;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ToolFinishInput;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_extension_api::ToolStartInput;
use codex_features::Feature;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

struct GatedNestedToolCompletion {
    started: Mutex<Option<oneshot::Sender<()>>>,
    finishing: CancellationToken,
    release: CancellationToken,
}

impl ToolLifecycleContributor for GatedNestedToolCompletion {
    fn on_tool_start<'a>(&'a self, input: ToolStartInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            if input.tool_name.name == "test_sync_tool"
                && matches!(
                    input.source,
                    codex_extension_api::ToolCallSource::CodeMode { .. }
                )
                && let Some(started) = self
                    .started
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
            {
                let _ = started.send(());
            }
        })
    }

    fn on_tool_finish<'a>(&'a self, input: ToolFinishInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            if input.tool_name.name == "test_sync_tool"
                && matches!(
                    input.source,
                    codex_extension_api::ToolCallSource::CodeMode { .. }
                )
            {
                self.finishing.cancel();
                self.release.cancelled().await;
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_shutdown_keeps_writer_until_accepted_nested_tool_completion_finishes() -> Result<()>
{
    skip_if_no_network!(Ok(()));
    let (started, started_rx) = oneshot::channel();
    let observer = Arc::new(GatedNestedToolCompletion {
        started: Mutex::new(Some(started)),
        finishing: CancellationToken::new(),
        release: CancellationToken::new(),
    });
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.tool_lifecycle_contributor(observer.clone());
    let server = responses::start_mock_server().await;
    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("durable-code-mode"),
            ev_custom_tool_call(
                "nested-call",
                "exec",
                "await tools.test_sync_tool({ sleep_after_ms: 60_000 });",
            ),
            ev_completed("durable-code-mode"),
        ]),
    )
    .await;
    let mut builder = test_codex()
        .with_model("test-gpt-5.1-codex")
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            let _ = config.features.enable(Feature::CodeMode);
            let _ = config.features.enable(Feature::CodeModeInterrupt);
        });
    let test = builder.build_with_auto_env(&server).await?;
    let thread_id = test.session_configured.thread_id;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "start a nested tool".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    tokio::time::timeout(Duration::from_secs(10), started_rx).await??;
    let shutdown = test.codex.shutdown_durably_and_wait();
    tokio::pin!(shutdown);
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            result = &mut shutdown => panic!("shutdown finished before nested completion: {result:?}"),
            _ = observer.finishing.cancelled() => {}
        }
    }).await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut shutdown)
            .await
            .is_err()
    );
    assert!(
        test.thread_store
            .reserve_thread_writers(vec![thread_id])
            .await
            .is_err()
    );
    observer.release.cancel();
    tokio::time::timeout(Duration::from_secs(30), &mut shutdown).await??;
    let _writer = test
        .thread_store
        .reserve_thread_writers(vec![thread_id])
        .await?;
    Ok(())
}
