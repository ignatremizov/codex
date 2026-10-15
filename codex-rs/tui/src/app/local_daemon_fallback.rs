//! Cold tree recovery after the connected local daemon exits, never a live graph transfer.
//!
//! Startup dependencies retain the launch's credentials, environment and network-policy owners.
//! A socket-bound process observer proves death; ordinary resume still owns writer exclusion,
//! canonical history reconstruction and validation of persisted agent ownership.

use super::App;
use super::reconnect::Reconnected;
use crate::AppServerTarget;
use crate::app_server_session::AppServerSession;
use crate::app_server_session::AppServerStartedThread;
use crate::app_server_session::ResumeModelSettings;
use crate::legacy_core::config::Config;
use crate::local_settings::LocalSettings;
use codex_app_server_client::AppServerClient;
use codex_app_server_client::EmbeddedNetworkPolicy;
use codex_app_server_client::EnvironmentManager;
use codex_app_server_client::PreparedLocalTreeRecovery;
use codex_app_server_client::StateDbHandle;
use codex_app_server_protocol::TurnStatus;
use codex_arg0::Arg0DispatchPaths;
use codex_config::CloudConfigBundleLoader;
use codex_config::LoaderOverrides;
use codex_protocol::ThreadId;
use color_eyre::eyre::Result;
use color_eyre::eyre::WrapErr;
use std::sync::Arc;

/// Captured once at launch, rather than rediscovering credentials or executors after a failure.
#[derive(Clone)]
pub(crate) struct EmbeddedAppServerRecovery {
    pub(crate) arg0_paths: Arg0DispatchPaths,
    pub(crate) config: Config,
    pub(crate) cli_kv_overrides: Vec<(String, toml::Value)>,
    pub(crate) loader_overrides: LoaderOverrides,
    pub(crate) strict_config: bool,
    pub(crate) cloud_config_bundle: CloudConfigBundleLoader,
    pub(crate) feedback: codex_feedback::CodexFeedback,
    pub(crate) log_db: Option<codex_state::log_db::LogDbLayer>,
    pub(crate) environment_manager: Arc<EnvironmentManager>,
    pub(crate) network_policy: EmbeddedNetworkPolicy,
}

pub(super) struct LocalDaemonFallback {
    pub(super) startup: EmbeddedAppServerRecovery,
    pub(super) peer: codex_uds::PeerProcess,
}

/// Idle restored runtimes; continuation is consumed only after the UI adopts this server.
pub(super) struct RecoveredLocalTree {
    pub(super) state_db: StateDbHandle,
    pub(super) plan: PreparedLocalTreeRecovery,
    pub(super) background_threads: Vec<AppServerStartedThread>,
}

impl LocalDaemonFallback {
    pub(super) async fn recover(
        &self,
        local_settings: &LocalSettings,
        thread_id: ThreadId,
        deadline: tokio::time::Instant,
    ) -> Result<Reconnected> {
        let startup = &self.startup;
        let mut config = startup.config.clone();
        startup.network_policy.activate(&mut config);
        let state_db =
            crate::init_state_db_for_app_server_target(&config, &AppServerTarget::Embedded)
                .await?
                .ok_or_else(|| color_eyre::eyre::eyre!("embedded recovery requires local state"))?;
        let client = crate::start_embedded_app_server(
            startup.arg0_paths.clone(),
            config.clone(),
            startup.cli_kv_overrides.clone(),
            startup.loader_overrides.clone(),
            startup.strict_config,
            startup.cloud_config_bundle.clone(),
            startup.feedback.clone(),
            startup.log_db.clone(),
            Some(state_db.clone()),
            Arc::clone(&startup.environment_manager),
            startup.network_policy.clone(),
        )
        .await?;
        let mut session = AppServerSession::new(
            AppServerClient::InProcess(client),
            AppServerTarget::Embedded.thread_params_mode(),
        )
        .with_local_codex_home(&config.codex_home);
        // No old MCP connection, dynamic-tool capabilities or pending requests cross this
        // boundary. The thread id selects durable history, not an instruction to replay work.
        // Once startup has produced a runtime owner, a timeout must shut it down
        // explicitly rather than leave an admitted resume running without a client.
        let restored = tokio::time::timeout_at(deadline, async {
            let bootstrap = session.bootstrap(&config).await?;
            let plan = session.prepare_local_tree_recovery(thread_id).await?;
            let mut background_threads = Vec::new();
            for id in plan
                .thread_ids()
                .iter()
                .copied()
                .filter(|id| *id != thread_id)
            {
                background_threads.push(
                    session
                        .resume_thread(
                            local_settings,
                            config.clone(),
                            id,
                            ResumeModelSettings::PreserveExistingThread,
                        )
                        .await?,
                );
            }
            let thread = session
                .resume_thread(
                    local_settings,
                    config,
                    thread_id,
                    ResumeModelSettings::PreserveExistingThread,
                )
                .await?;
            Ok::<_, color_eyre::Report>((bootstrap, thread, plan, background_threads))
        })
        .await
        .wrap_err("Embedded history recovery timed out")
        .and_then(std::convert::identity);
        match restored {
            Ok((bootstrap, mut thread, plan, mut background_threads)) => {
                // Durable "in progress" describes the lost runtime. Show it as interrupted
                // until an actual admission from the replacement produces fresh live events.
                for started in std::iter::once(&mut thread).chain(&mut background_threads) {
                    for turn in &mut started.turns {
                        if turn.status == TurnStatus::InProgress {
                            turn.status = TurnStatus::Interrupted;
                        }
                    }
                }
                Ok(Reconnected {
                    session,
                    bootstrap,
                    thread: Some(thread),
                    local_recovery: Some(RecoveredLocalTree {
                        state_db,
                        plan,
                        background_threads,
                    }),
                })
            }
            Err(error) => {
                if let Err(shutdown_error) = session.shutdown().await {
                    tracing::warn!(%shutdown_error, "failed to shut down rejected local recovery");
                }
                Err(error).wrap_err(
                    "Local server exited, but this conversation could not be restored in this TUI. No input was resent",
                )
            }
        }
    }
}

impl App {
    pub(super) async fn adopt_recovered_background_threads(
        &mut self,
        threads: Vec<AppServerStartedThread>,
    ) {
        for started in threads {
            let id = started.session.thread_id;
            if started.is_subagent {
                self.agent_navigation.mark_subagent(id);
            }
            self.agent_navigation.upsert(
                id, /*agent_nickname*/ None, /*agent_role*/ None,
                /*is_closed*/ false,
            );
            self.agent_navigation.mark_stopped(id);
            if self.primary_thread_id == Some(id) {
                self.primary_session_configured = Some(started.session.clone());
            }
            let channel = self.ensure_thread_channel(id);
            channel.mark_live();
            channel
                .store
                .lock()
                .await
                .set_session(started.session, started.turns);
        }
    }

    pub(super) async fn continue_recovered_local_tree(
        &mut self,
        server: &AppServerSession,
        plan: PreparedLocalTreeRecovery,
    ) {
        for warning in plan.warnings() {
            self.chat_widget.add_error_message(warning.clone());
        }
        match server.continue_local_tree_recovery(plan).await {
            Ok(outcome) => {
                let count = outcome.resumed_turns.len();
                for id in outcome.resumed_turns {
                    self.agent_navigation.mark_running(id);
                }
                self.chat_widget.add_info_message(
                    format!("Local background server exited; recovered this session and its agents in this TUI. Resumed {count} interrupted turns. Pending inputs were not resent."),
                    /*hint*/ None,
                );
                for (id, reason) in outcome.paused_threads {
                    self.chat_widget
                        .add_error_message(format!("Agent {id} remains paused: {reason}"));
                }
            }
            Err(error) => {
                // Admission may already have started some turns. Never retry the consumed
                // plan or turn an uncertain acknowledgement into another recovery request.
                self.chat_widget.add_error_message(format!(
                    "Local server recovery restored saved history, but automatic continuation could not be confirmed: {error}. No recovery request was retried."
                ));
            }
        }
    }
}
