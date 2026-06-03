//! A decoder attempt owns startup and shutdown, including when its caller disappears.
//! This deliberately does not alter the lifecycle of ordinary delegates.

use std::sync::Arc;

use crate::agents_md_manager::SessionInstructions;
use crate::config::Config;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::ForkPersistence;
use crate::session::GitEnrichmentPolicy;
use crate::session::SessionSpawnArgs;
use crate::session::session::Session;
use crate::session::startup::SessionStartup;
use crate::session::turn_context::TurnContext;
use codex_extension_api::ExtensionDataInit;
use codex_extension_api::SessionIsolation;
use codex_extension_api::ToolPolicy;
use codex_history::InitialHistory;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::turn_input::TurnInputMode;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::turn_input::TurnStartOptions;
use futures::FutureExt;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Only this delegate suppresses automatic compaction of the checkpoint it is decoding.
pub(crate) struct CompactionDecoder;

/// A lost owner cannot prove that its helper actor terminated; callers must not retry.
#[derive(Debug, thiserror::Error)]
#[error("decoder lifetime worker was lost before confirming cleanup: {0}")]
pub(crate) struct DecoderWorkerLost(tokio::task::JoinError);

pub(crate) struct DecoderRequest {
    pub(crate) config: Config,
    pub(crate) parent: Arc<Session>,
    pub(crate) turn: Arc<TurnContext>,
    pub(crate) history: InitialHistory,
    pub(crate) cancellation: CancellationToken,
    pub(crate) deadline: Instant,
}

pub(crate) async fn run(request: DecoderRequest) -> anyhow::Result<String> {
    run_with_startup(request, Arc::new(SessionStartup::default())).await
}

async fn run_with_startup(
    request: DecoderRequest,
    startup: Arc<SessionStartup>,
) -> anyhow::Result<String> {
    let cancellation = request.cancellation.child_token();
    // Dropping the caller cannot abandon partially acquired session resources.
    let _caller_guard = cancellation.clone().drop_guard();
    let worker = tokio::spawn(async move {
        let result = {
            let attempt =
                std::panic::AssertUnwindSafe(decode(&request, Arc::clone(&startup))).catch_unwind();
            tokio::pin!(attempt);
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(anyhow::anyhow!("decoder cancelled")),
                _ = tokio::time::sleep_until(request.deadline) => {
                    Err(anyhow::anyhow!("decoder startup/inference timed out"))
                }
                result = &mut attempt => result
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("decoder attempt panicked"))),
            }
        };
        // The initialization future is dropped before cleanup observes startup's resources.
        startup.cleanup().await;
        if let Some(io) = startup.io.get() {
            // cleanup's shutdown submission can fail. Only this future proves actor termination.
            io.session_loop_termination.clone().await;
        }
        result
    });
    worker.await.map_err(DecoderWorkerLost)?
}

fn decode(
    request: &DecoderRequest,
    startup: Arc<SessionStartup>,
) -> futures::future::BoxFuture<'_, anyhow::Result<String>> {
    // Erase the future at the recursive session-spawn boundary.
    Box::pin(async move {
        let parent = &request.parent;
        let turn = &request.turn;
        let mut config = request.config.clone();
        config.model_provider.supports_websockets &=
            parent.services.model_client.responses_websocket_enabled();
        let mut thread_extension_init = ExtensionDataInit::default();
        thread_extension_init.insert(SessionIsolation::Isolated);
        thread_extension_init.insert(CompactionDecoder);
        thread_extension_init.insert(ToolPolicy {
            allowed_tools: Some(Vec::new()),
            ..Default::default()
        });
        // Spell out this isolated boundary rather than expanding the ordinary delegate API with
        // decoder-specific switches. Shared services are capabilities; none select an environment
        // or contribute tools/instructions to this session.
        let (_session, io) = Session::spawn(SessionSpawnArgs {
            startup: Some(startup),
            config,
            allow_provider_model_fallback: false,
            instructions: SessionInstructions::default(),
            installation_id: parent.installation_id.clone(),
            auth_manager: Arc::clone(&parent.services.auth_manager),
            models_manager: parent.services.models_manager.clone(),
            git_root_discovery: Arc::clone(&parent.services.git_root_discovery),
            environment_manager: parent.services.turn_environments.environment_manager(),
            skills_service: Arc::clone(&parent.services.skills_service),
            plugins_manager: Arc::clone(&parent.services.plugins_manager),
            mcp_manager: Arc::clone(&parent.services.mcp_manager),
            code_mode_session_provider: parent.services.code_mode_service.session_provider(),
            extensions: codex_extension_api::empty_extension_registry(),
            conversation_history: request.history.clone(),
            disabled_plugin_ids: None,
            requested_history_mode: None,
            fork_persistence: ForkPersistence::Copied,
            session_source: SessionSource::SubAgent(SubAgentSource::Compact),
            forked_from_thread_id: None,
            parent_thread_id: Some(parent.thread_id),
            thread_source: Some(ThreadSource::Subagent),
            originator: turn.originator.clone(),
            agent_control: parent.services.agent_control.clone(),
            dynamic_tools: Vec::new(),
            metrics_service_name: None,
            inherited_exec_policy: None,
            inherited_environments: Some(TurnEnvironmentSnapshot::default()),
            parent_rollout_thread_trace: codex_rollout_trace::ThreadTraceContext::disabled(),
            user_shell_override: None,
            parent_trace: None,
            environment_selections: Vec::new(),
            thread_extension_init,
            client_mcp_extensions: Default::default(),
            reserved_thread_id: None,
            analytics_events_client: Some(parent.services.analytics_events_client.clone()),
            image_store: Arc::clone(&parent.services.image_store),
            thread_store: Arc::clone(&parent.services.thread_store),
            attestation_provider: parent.services.attestation_provider.clone(),
            external_time_provider: Some(Arc::clone(&parent.services.time_provider)),
            inherited_multi_agent_version: Some(MultiAgentVersion::Disabled),
            git_enrichment_policy: GitEnrichmentPolicy::Skip,
            windows_sandbox_proxy_settings_mode:
                codex_sandboxing::WindowsSandboxProxySettingsMode::Preserve,
        })
        .await?;
        let submission = io
            .submit_turn_input(
                TurnInputRequest::user_input(Vec::new()).on_start(TurnStartOptions {
                    parent_turn_id: Some(turn.sub_id.clone()),
                    root_turn_id: turn.turn_metadata_state.root_turn_id(),
                    ..Default::default()
                }),
                TurnInputMode::StartIfIdle,
            )
            .await?;
        if !matches!(submission, TurnInputSubmission::Started { .. }) {
            anyhow::bail!("decoder turn input was not started: {submission:?}");
        }
        loop {
            match io.next_event().await?.msg {
                EventMsg::TurnComplete(completed) => {
                    if let Some(error) = completed.error {
                        anyhow::bail!("{}", error.message);
                    }
                    return completed
                        .last_agent_message
                        .ok_or_else(|| anyhow::anyhow!("decoder returned no text"));
                }
                EventMsg::TurnAborted(aborted) => {
                    anyhow::bail!("decoder turn was aborted: {:?}", aborted.reason);
                }
                EventMsg::Error(error) if error.affects_turn_status() => {
                    anyhow::bail!("{}", error.message);
                }
                // Lifecycle/streaming events remain private to the decoder.
                _ => {}
            }
        }
    })
}

#[cfg(test)]
#[path = "compaction_tests.rs"]
mod tests;
