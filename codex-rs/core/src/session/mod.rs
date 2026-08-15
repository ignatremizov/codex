use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Debug;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use crate::agent::AgentControl;
use crate::agent::AgentStatus;
use crate::agent::agent_status_from_event;
use crate::agent::control::AgentTerminalPresentation;
use crate::agent::status::is_final;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::attestation::AttestationProvider;
use crate::compact;
use crate::compact::CompactedHistoryMetadata;
use crate::config::ManagedFeatures;
use crate::config::resolve_tool_suggest_config_from_layer_stack;
use crate::context::ContextualUserFragment;
use crate::context::DeveloperInstructions;
use crate::context::GuardianPolicy;
use crate::context::ManagedDeveloperInstructions;
use crate::context::ModelSwitchInstructions;
use crate::context::MultiAgentRoleInstructions;
use crate::context::NetworkRuleSaved;
use crate::context::RecommendedPluginsInstructions;
use crate::context::world_state::WorldState;
use crate::current_time::TimeProvider;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::exec_policy::BANNED_PREFIX_SUGGESTIONS;
use crate::exec_policy::ExecPolicyManager;
use crate::exec_policy::default_policy_path;
use crate::image_preparation::ImagePreparationMode;
use crate::image_preparation::ImageResizeNoticeMode;
use crate::image_preparation::prepare_response_items as prepare_image_response_items;
use crate::image_preparation::unified_image_budget_enabled;
use crate::parse_turn_item;
use crate::realtime_conversation::RealtimeConversationManager;
use crate::realtime_history::RealtimeEventOrder;
use crate::session::step_context::StepContext;
use crate::session::step_settings::ResolvedStepSettings;
use crate::session::step_settings::StepSettings;
use crate::session::turn_context::TurnEnvironment;
use crate::session_prefix::format_inter_agent_completion_message;
use crate::skills_load_input_from_config;
use crate::stream_events_utils::mark_thread_memory_mode_polluted_if_external_context;
use crate::turn_metadata::TurnMetadataState;
use crate::turn_timing::now_unix_timestamp_ms;
use async_channel::Receiver;
use async_channel::Sender;
use chrono::Local;
use chrono::Utc;
use codex_analytics::AnalyticsEventsClient;
use codex_analytics::ImagePreparationFact;
use codex_analytics::ImagePreparationMetadata;
use codex_analytics::SubAgentThreadStartedInput;
use codex_analytics::TurnCodexErrorFact;
use codex_async_utils::OrCancelExt;
use codex_connectors::connector_runtime_context_key;
use codex_context_fragments::RenderedFragment;
use codex_exec_server::Environment;
use codex_exec_server::EnvironmentManager;
use codex_execpolicy::prefix_rule_migration;
use codex_extension_api::ConversationHistorySnapshot;
use codex_extension_api::ExtensionDataInit;
use codex_extension_api::LoadedUserInstructions;
use codex_extension_api::PromptSlot;
use codex_extension_api::TurnContextContributionInput;
use codex_extension_api::TurnInputContributionAcknowledgement;
use codex_features::FEATURES;
use codex_features::Feature;
use codex_features::unstable_features_warning_event;
use codex_history::RolloutItem;
use codex_hooks::Hooks;
use codex_hooks::HooksConfig;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::auth_env_telemetry::collect_auth_env_telemetry;
use codex_mcp::McpResourceClient;
use codex_mcp::McpRuntime;
use codex_mcp::McpRuntimeContext;
use codex_mcp::McpRuntimeInput;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::SharedModelsManager;
use codex_network_proxy::NetworkProxy;
use codex_network_proxy::NetworkProxyAuditMetadata;
use codex_network_proxy::normalize_host;
use codex_otel::current_span_trace_id;
use codex_otel::current_span_w3c_trace_context;
use codex_otel::set_parent_from_w3c_trace_context;
use codex_protocol::ResponseUsageMetadata;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::approvals::ElicitationRequest;
use codex_protocol::approvals::ElicitationRequestEvent;
use codex_protocol::approvals::ExecPolicyAmendment;
use codex_protocol::approvals::NetworkPolicyAmendment;
use codex_protocol::approvals::NetworkPolicyRuleAction;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::EnteredReviewModeItem;
use codex_protocol::items::SubAgentActivityItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::ActivePermissionProfile;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::BaseInstructionsProvenance;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::SandboxEnforcement;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::FileSystemSandboxPolicyContext;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::AgentInputPresentation;
use codex_protocol::protocol::ContextCompactionStatusEvent;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::HasLegacyEvent;
use codex_protocol::protocol::HistoryPosition;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::MULTI_AGENT_MODE_OPEN_TAG;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::NonSteerableTurnKind;
use codex_protocol::protocol::RawResponseItemEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentActivityKind;
use codex_protocol::protocol::SubAgentCompletionModelVisibility;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::TurnContextNetworkItem;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::W3cTraceContext;
use codex_protocol::protocol::WorldStateItem;
use codex_protocol::protocol::is_sub_agent_completion_context_response_item_id;
use codex_protocol::protocol::new_attributed_agent_message_response_item_id;
use codex_protocol::request_permissions::PermissionGrantScope;
use codex_protocol::request_permissions::RequestPermissionProfile;
use codex_protocol::request_permissions::RequestPermissionsArgs;
use codex_protocol::request_permissions::RequestPermissionsEvent;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_rmcp_client::ElicitationResponse;
use codex_rollout::state_db;
use codex_rollout_trace::AgentResultTracePayload;
use codex_rollout_trace::ThreadStartedTraceMetadata;
use codex_rollout_trace::ThreadTraceContext;
use codex_sandboxing::policy_transforms::intersect_permission_profiles_with_context;
use codex_shell_command::parse_command::parse_command;
use codex_terminal_detection::user_agent;
use codex_thread_store::CreateThreadParams;
use codex_thread_store::LiveThread;
use codex_thread_store::LiveThreadInitGuard;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::PersistContext;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::ResumeThreadParams;
use codex_thread_store::ThreadPersistenceMetadata;
use codex_thread_store::ThreadStore;
use codex_thread_store::ThreadStoreError;
use codex_utils_audio::prepare_response_items as prepare_audio_response_items;
use codex_utils_git_discovery::GitRootDiscovery;
use codex_utils_path_uri::PathUri;
use futures::future::BoxFuture;
use futures::future::Shared;
use futures::prelude::*;
use rmcp::model::RequestId;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::RwLock;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use toml::Value as TomlValue;
use tracing::Instrument;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing::instrument;
use tracing::warn;
use uuid::Uuid;

use crate::client::ModelClient;
use crate::codex_thread::CodexThreadSettingsOverrides;
use crate::codex_thread::ThreadConfigSnapshot;
#[cfg(test)]
use crate::compact::collect_user_messages;
use crate::config::Config;
use crate::config::ConstraintResult;
use crate::config::PermissionProfileSnapshot;
use crate::config::PermissionProfileState;
use crate::config::StartedNetworkProxy;
use crate::config::resolve_web_search_mode_for_turn;
use crate::context_manager::ContextManager;
use crate::context_manager::HistoryReplacement;
use crate::thread_rollout_truncation::initial_history_has_prior_user_turns;
use codex_config::CONFIG_TOML_FILE;
use codex_config::ConfigLayerSource;
use codex_model_provider::create_model_provider;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
#[cfg(test)]
use codex_protocol::exec_output::StreamOutput;

mod code_mode_warning;
mod compacted_media_repair;
pub(crate) mod context_window;
mod environment;
pub(crate) mod extension_metrics;
mod handlers;
pub(crate) use handlers::inter_agent_communication;
mod inject;
mod input_queue;
mod inter_agent_communication;
mod mcp;
mod mcp_prewarm;
mod mcp_refresh;
mod mcp_runtime;
pub(crate) mod multi_agents;
mod realtime_history;
mod response_observation;
mod review;
mod rollout_budget;
mod rollout_reconstruction;
#[allow(clippy::module_inception)]
pub(crate) mod session;
mod step_activation;
pub(crate) mod step_context;
pub(crate) mod step_settings;
mod sub_agent_completion;
mod thread_settings;
pub(crate) use sub_agent_completion::TerminalStatusEvent;
pub(crate) use sub_agent_completion::TerminalStatusSubscription;
pub(crate) mod time_reminder;
mod token_budget;
pub(crate) mod turn;
pub(crate) mod turn_context;
mod turn_input;
mod turn_suspension;
mod world_state;
use self::code_mode_warning::unsupported_code_mode_warning;
#[cfg(test)]
use self::handlers::submission_dispatch_span;
use self::handlers::submission_loop;
pub(crate) use self::input_queue::InputQueueActivity;
pub(crate) use self::input_queue::PendingInputFollowUp;
pub(crate) use self::input_queue::PromptInputKind;
pub(crate) use self::input_queue::PromptTurnDraft;
pub(crate) use self::input_queue::TurnInput;
pub(crate) use self::input_queue::TurnInputQueue;
pub(crate) use self::input_queue::classify_pending_input_follow_up;
use self::input_queue::is_mcp_server_use_context_input_item;
pub(crate) use self::response_observation::AgentResponseEvent;
pub(crate) use self::response_observation::AgentResponseSubscription;
pub(crate) use self::response_observation::InputTurnAdmissionPolicy;
pub(crate) use self::response_observation::InputTurnAdmissionResolution;
pub(crate) use self::response_observation::SubmittedInputTurn;
pub(crate) use self::response_observation::agent_response_events_from_rollout;
use self::review::spawn_review_thread;
use self::session::AppServerClientMetadata;
use self::session::Session;
use self::session::SessionConfiguration;
use self::session::SessionSettingsCommit;
pub(crate) use self::session::SessionSettingsUpdate;
#[cfg(test)]
use self::turn::AssistantMessageStreamParsers;
use self::turn::agent_message_text;
#[cfg(test)]
use self::turn::collect_explicit_app_ids_from_skill_items;
use self::turn::realtime_text_for_event;
use self::turn_context::TurnContext;
#[cfg(test)]
mod rollout_reconstruction_tests;

#[derive(Clone, Copy)]
enum EventPersistence {
    Skip,
    Queued,
    Flushed,
}

#[derive(Clone, Copy)]
enum AgentResponseEventPublication {
    Skip,
    Publish,
}

struct RawEventDelivery {
    persisted: bool,
    delivered: bool,
}

impl RawEventDelivery {
    fn committed(&self) -> bool {
        self.persisted && self.delivered
    }
}

#[derive(Debug, PartialEq)]
pub enum SteerInputError {
    NoActiveTurn(Vec<UserInput>),
    ActiveTurnPresent { actual: String },
    ExpectedTurnMismatch { expected: String, actual: String },
    ActiveTurnNotSteerable { turn_kind: NonSteerableTurnKind },
    EmptyInput,
}

pub(crate) const QUEUED_INPUT_ACTIVE_ERROR_PREFIX: &str =
    "queued input requires an idle target; active turn is ";
pub(crate) const STEER_ONLY_TARGET_ENDED_ERROR: &str =
    "target turn ended before steer-only input admission";

/// Notes from the previous real user turn.
///
/// Conceptually this is the same role that `previous_model` used to fill, but
/// it can carry other prior-turn settings that matter when constructing
/// sensible state-change diffs or full-context reinjection, such as model
/// switches, compaction compatibility, or detecting a prior
/// `realtime_active -> false` transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreviousTurnSettings {
    pub(crate) model: String,
    pub(crate) comp_hash: Option<String>,
    pub(crate) realtime_active: Option<bool>,
}

use crate::exec_policy::ExecPolicyUpdateError;
use crate::guardian::GuardianReviewSessionManager;
use crate::mcp::McpEnvironmentScope;
use crate::mcp::McpManager;
use crate::mcp::McpThreadIdentity;
use crate::network_policy_decision::execpolicy_network_rule_amendment;
use crate::rollout::map_session_init_error;
use crate::session_startup_prewarm::SessionStartupPrewarmHandle;
use crate::shell;
use crate::state::ActiveTurn;
use crate::state::AutoCompactWindowIds;
use crate::state::AutoCompactWindowSnapshot;
use crate::state::PendingRequestPermissions;
use crate::state::SessionServices;
use crate::state::SessionState;
#[cfg(test)]
use crate::stream_events_utils::HandleOutputCtx;
#[cfg(test)]
use crate::stream_events_utils::handle_output_item_done;
use crate::tasks::ReviewTask;
use crate::tools::ApprovalContext;
use crate::tools::network_approval::NetworkApprovalService;
use crate::tools::network_approval::build_blocked_request_observer;
use crate::tools::network_approval::build_network_policy_decider;
#[cfg(test)]
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::sandboxing::ApprovalAction;
use crate::tools::sandboxing::ApprovalStore;
use crate::turn_timing::TurnTimingState;
use crate::turn_timing::record_turn_ttfm_metric;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::windows_sandbox::WindowsSandboxLevelExt;
use codex_core_plugins::PluginCommandAttribution;
use codex_core_plugins::PluginsManager;
use codex_core_plugins::RecommendedPluginCandidatesInput;
use codex_git_utils::get_git_repo_root;
use codex_history::CompactedItem;
use codex_history::InitialHistory;
use codex_history::ResponseItemEnvelope;
use codex_mcp::McpConfig;
use codex_mcp::ToolInfo as McpToolInfo;
use codex_mcp::effective_mcp_servers;
use codex_otel::SessionTelemetry;
use codex_otel::THREAD_STARTED_METRIC;
use codex_otel::TelemetryAuthMode;
use codex_protocol::ResponseItemId;
use codex_protocol::approvals::ExecApprovalKind;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::config_types::Settings;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::models::LocalImagePreparation;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::ApplyPatchApprovalRequestEvent;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::DeprecationNoticeEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecApprovalRequestEvent;
use codex_protocol::protocol::ModelRerouteEvent;
use codex_protocol::protocol::ModelRerouteReason;
use codex_protocol::protocol::ModelVerification;
use codex_protocol::protocol::ModelVerificationEvent;
use codex_protocol::protocol::NetworkApprovalContext;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RawResponseCompletedEvent;
use codex_protocol::protocol::RequestUserInputEvent;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_protocol::protocol::SessionNetworkProxyRuntime;
use codex_protocol::protocol::StreamErrorEvent;
use codex_protocol::protocol::Submission;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::TokenCountEvent;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::TokenUsageRecord;
use codex_protocol::protocol::TurnModerationMetadataEvent;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::turn_input::TurnInputMode;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::turn_input::TurnStartOptions;
use codex_protocol::user_input::UserInput;
use codex_skills_extension::HostSkillsService;
use codex_tools::ToolName;
use codex_tools::UnifiedExecShellMode;
use codex_utils_absolute_path::AbsolutePathBuf;
#[cfg(test)]
use codex_utils_stream_parser::ProposedPlanSegment;

/// Queue and lifecycle endpoints for a running [`Session`].
///
/// Runtime state lives on `Session`; keeping these endpoints separate lets all
/// submission senders be dropped to terminate the session loop. The shared
/// completion future observes that shutdown.
pub(crate) struct SessionIo {
    pub(crate) tx_sub: Sender<Submission>,
    pub(crate) rx_event: Receiver<Event>,
    /// Serializes enqueueing with rollback reservation and reload quarantine transitions.
    pub(crate) submission_admission: Arc<SubmissionAdmission>,
    // Last known status of the agent.
    pub(crate) agent_status: watch::Receiver<AgentStatus>,
    // Shared future for the background submission loop completion so multiple
    // callers can wait for shutdown.
    pub(crate) session_loop_termination: SessionLoopTermination,
}

#[derive(Default)]
pub(crate) struct SubmissionAdmission {
    send_lock: Mutex<()>,
    shutdown_sequence_lock: Mutex<()>,
    state: StdMutex<SubmissionAdmissionState>,
    shutdown_pending: AtomicBool,
    completion_delivery_admission_closed: AtomicBool,
    accepted_completion_deliveries: AtomicUsize,
    changed: Notify,
    completion_delivery_changed: Notify,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SubmissionAdmissionState {
    #[default]
    Ready,
    RollbackPending,
    RollbackEventPending,
    ReloadRequired,
}

impl SubmissionAdmission {
    pub(crate) fn rollback_ready_to_publish(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(*state, SubmissionAdmissionState::RollbackPending) {
            *state = SubmissionAdmissionState::RollbackEventPending;
        }
    }

    pub(crate) fn rollback_completed(&self) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) =
            SubmissionAdmissionState::Ready;
        self.changed.notify_waiters();
    }

    pub(crate) fn rollback_requires_reload(&self) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) =
            SubmissionAdmissionState::ReloadRequired;
        self.changed.notify_waiters();
    }

    pub(crate) async fn wait_for_completion_submission(&self) -> bool {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self
                .shutdown_pending
                .load(std::sync::atomic::Ordering::Acquire)
                || self
                    .completion_delivery_admission_closed
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                return false;
            }
            let state = *self.state.lock().unwrap_or_else(PoisonError::into_inner);
            match state {
                SubmissionAdmissionState::Ready => return true,
                SubmissionAdmissionState::ReloadRequired => return false,
                SubmissionAdmissionState::RollbackPending
                | SubmissionAdmissionState::RollbackEventPending => {}
            }
            changed.as_mut().await;
        }
    }

    pub(crate) async fn wait_for_accepted_completion_submission(&self) -> bool {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let state = *self.state.lock().unwrap_or_else(PoisonError::into_inner);
            match state {
                SubmissionAdmissionState::Ready => return true,
                SubmissionAdmissionState::ReloadRequired => return false,
                SubmissionAdmissionState::RollbackPending
                | SubmissionAdmissionState::RollbackEventPending => {}
            }
            changed.as_mut().await;
        }
    }

    pub(crate) fn response_observation_delivery_can_retry(&self) -> bool {
        self.response_observation_accepted_delivery_can_retry()
            && !self
                .shutdown_pending
                .load(std::sync::atomic::Ordering::Acquire)
            && !self
                .completion_delivery_admission_closed
                .load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn response_observation_accepted_delivery_can_retry(&self) -> bool {
        // An accepted completion reserved admission before shutdown closed new deliveries. Its
        // admission counter intentionally keeps shutdown waiting, so only reload quarantine makes
        // the retry ineligible; session-loop termination is checked by AgentControl.
        !matches!(
            *self.state.lock().unwrap_or_else(PoisonError::into_inner),
            SubmissionAdmissionState::ReloadRequired
        )
    }

    pub(crate) fn try_accept_completion_delivery(
        self: &Arc<Self>,
    ) -> Option<AcceptedCompletionDelivery> {
        if self
            .completion_delivery_admission_closed
            .load(std::sync::atomic::Ordering::Acquire)
            || self
                .shutdown_pending
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return None;
        }
        self.accepted_completion_deliveries
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        if self
            .completion_delivery_admission_closed
            .load(std::sync::atomic::Ordering::Acquire)
            || self
                .shutdown_pending
                .load(std::sync::atomic::Ordering::Acquire)
        {
            self.finish_accepted_completion_delivery();
            return None;
        }
        Some(AcceptedCompletionDelivery {
            admission: Arc::clone(self),
        })
    }

    async fn wait_for_accepted_completion_deliveries(&self) {
        loop {
            let changed = self.completion_delivery_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self
                .accepted_completion_deliveries
                .load(std::sync::atomic::Ordering::Acquire)
                == 0
            {
                return;
            }
            changed.as_mut().await;
        }
    }

    fn finish_accepted_completion_delivery(&self) {
        let previous = self
            .accepted_completion_deliveries
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        debug_assert!(previous > 0);
        if previous == 1 {
            self.completion_delivery_changed.notify_waiters();
        }
    }
}

pub(crate) struct AcceptedCompletionDelivery {
    admission: Arc<SubmissionAdmission>,
}

#[derive(Clone, Copy)]
pub(crate) enum CompletionSubmissionAdmission {
    Ordinary,
    Accepted,
}

#[derive(Clone, Copy)]
enum SubmissionAdmissionKind {
    Ordinary,
    AcceptedCompletion,
}

impl Drop for AcceptedCompletionDelivery {
    fn drop(&mut self) {
        self.admission.finish_accepted_completion_delivery();
    }
}

struct RollbackAdmissionGuard {
    admission: Arc<SubmissionAdmission>,
    armed: bool,
}

struct CompletionShutdownAdmissionGuard {
    admission: Arc<SubmissionAdmission>,
    armed: bool,
}

struct ShutdownAdmissionGuard {
    admission: Arc<SubmissionAdmission>,
    armed: bool,
}

impl RollbackAdmissionGuard {
    fn new(admission: Arc<SubmissionAdmission>) -> Self {
        Self {
            admission,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RollbackAdmissionGuard {
    fn drop(&mut self) {
        if self.armed {
            let mut state = self
                .admission
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if matches!(
                *state,
                SubmissionAdmissionState::RollbackPending
                    | SubmissionAdmissionState::RollbackEventPending
            ) {
                *state = SubmissionAdmissionState::Ready;
                self.admission.changed.notify_waiters();
            }
        }
    }
}

impl CompletionShutdownAdmissionGuard {
    fn new(admission: Arc<SubmissionAdmission>) -> Self {
        let armed = !admission
            .completion_delivery_admission_closed
            .swap(true, std::sync::atomic::Ordering::AcqRel);
        admission.changed.notify_waiters();
        Self { admission, armed }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CompletionShutdownAdmissionGuard {
    fn drop(&mut self) {
        if self.armed {
            self.admission
                .completion_delivery_admission_closed
                .store(false, std::sync::atomic::Ordering::Release);
            self.admission.changed.notify_waiters();
        }
    }
}

impl ShutdownAdmissionGuard {
    fn new(admission: Arc<SubmissionAdmission>) -> Self {
        Self {
            admission,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ShutdownAdmissionGuard {
    fn drop(&mut self) {
        if self.armed {
            self.admission
                .shutdown_pending
                .store(false, std::sync::atomic::Ordering::Release);
            self.admission.changed.notify_waiters();
        }
    }
}

pub(crate) type SessionLoopTermination = Shared<BoxFuture<'static, ()>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GitEnrichmentPolicy {
    Fresh,
    Skip,
}

/// Controls which fork history belongs in the newly created thread's own rollout.
pub(crate) enum ForkPersistence {
    Copied,
    Referenced {
        history_base: Option<HistoryPosition>,
        inherited_item_count: usize,
    },
}

pub(crate) struct SessionSpawnArgs {
    pub(crate) config: Config,
    pub(crate) allow_provider_model_fallback: bool,
    pub(crate) user_instructions: LoadedUserInstructions,
    pub(crate) installation_id: String,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) models_manager: SharedModelsManager,
    pub(crate) git_root_discovery: Arc<GitRootDiscovery>,
    pub(crate) environment_manager: Arc<EnvironmentManager>,
    pub(crate) skills_service: Arc<HostSkillsService>,
    pub(crate) plugins_manager: Arc<PluginsManager>,
    pub(crate) mcp_manager: Arc<McpManager>,
    pub(crate) code_mode_session_provider: Arc<dyn codex_code_mode::CodeModeSessionProvider>,
    pub(crate) extensions: Arc<codex_extension_api::ExtensionRegistry<crate::config::Config>>,
    pub(crate) conversation_history: InitialHistory,
    pub(crate) requested_history_mode: Option<ThreadHistoryMode>,
    pub(crate) fork_persistence: ForkPersistence,
    pub(crate) session_source: SessionSource,
    pub(crate) forked_from_thread_id: Option<ThreadId>,
    pub(crate) parent_thread_id: Option<ThreadId>,
    pub(crate) thread_source: Option<ThreadSource>,
    pub(crate) originator: String,
    pub(crate) agent_control: AgentControl,
    pub(crate) dynamic_tools: Vec<DynamicToolSpec>,
    pub(crate) metrics_service_name: Option<String>,
    pub(crate) inherited_exec_policy: Option<Arc<ExecPolicyManager>>,
    pub(crate) inherited_environments: Option<TurnEnvironmentSnapshot>,
    /// Parent rollout trace used only to derive fresh spawned child traces.
    ///
    /// Root sessions and non-thread-spawn subagents pass a disabled context;
    /// `Session::new` creates the root trace itself when rollout tracing is enabled.
    pub(crate) parent_rollout_thread_trace: ThreadTraceContext,
    pub(crate) user_shell_override: Option<shell::Shell>,
    pub(crate) parent_trace: Option<W3cTraceContext>,
    pub(crate) environment_selections: Vec<TurnEnvironmentSelection>,
    pub(crate) thread_extension_init: ExtensionDataInit,
    pub(crate) client_mcp_extensions: ClientMcpExtensions,
    pub(crate) reserved_thread_id: Option<ThreadId>,
    pub(crate) analytics_events_client: Option<AnalyticsEventsClient>,
    pub(crate) thread_store: Arc<dyn ThreadStore>,
    pub(crate) attestation_provider: Option<Arc<dyn AttestationProvider>>,
    pub(crate) external_time_provider: Option<Arc<dyn TimeProvider>>,
    pub(crate) inherited_multi_agent_version: Option<MultiAgentVersion>,
    pub(crate) git_enrichment_policy: GitEnrichmentPolicy,
    pub(crate) windows_sandbox_proxy_settings_mode:
        codex_sandboxing::WindowsSandboxProxySettingsMode,
}

pub(crate) fn resolve_multi_agent_version(
    conversation_history: &InitialHistory,
    inherited_multi_agent_version: Option<MultiAgentVersion>,
) -> Option<MultiAgentVersion> {
    if inherited_multi_agent_version == Some(MultiAgentVersion::Disabled) {
        return Some(MultiAgentVersion::Disabled);
    }

    conversation_history
        .get_multi_agent_version()
        .or(inherited_multi_agent_version)
        .or(match conversation_history {
            InitialHistory::New | InitialHistory::Cleared => None,
            // Threads created before runtime metadata existed keep the legacy V1 tool surface.
            InitialHistory::Resumed(_) | InitialHistory::Forked(_) => Some(MultiAgentVersion::V1),
        })
}

pub(crate) const INITIAL_SUBMIT_ID: &str = "";
pub(crate) const SUBMISSION_CHANNEL_CAPACITY: usize = 512;
const CYBER_VERIFY_URL: &str = "https://chatgpt.com/cyber";
const CYBER_SAFETY_URL: &str = "https://developers.openai.com/codex/concepts/cyber-safety";

pub(crate) fn saturating_instant_add_ms(
    now: tokio::time::Instant,
    timeout_ms: u64,
) -> tokio::time::Instant {
    if let Some(deadline) = now.checked_add(std::time::Duration::from_millis(timeout_ms)) {
        return deadline;
    }

    let mut lower = 0;
    let mut upper = timeout_ms;
    while lower < upper {
        let midpoint = lower + (upper - lower).div_ceil(2);
        if now
            .checked_add(std::time::Duration::from_millis(midpoint))
            .is_some()
        {
            lower = midpoint;
        } else {
            upper = midpoint - 1;
        }
    }
    now.checked_add(std::time::Duration::from_millis(lower))
        .unwrap_or(now)
}

impl Session {
    /// Spawn and initialize a new session.
    /// Hide the concrete startup future from callers while keeping initialization lazy.
    #[inline(never)]
    pub(crate) fn spawn(
        args: SessionSpawnArgs,
    ) -> BoxFuture<'static, CodexResult<(Arc<Self>, SessionIo)>> {
        Box::pin(async move {
            let parent_trace = match args.parent_trace {
                Some(trace) => {
                    if codex_otel::context_from_w3c_trace_context(&trace).is_some() {
                        Some(trace)
                    } else {
                        warn!("ignoring invalid thread spawn trace carrier");
                        None
                    }
                }
                None => None,
            };
            let thread_spawn_span = info_span!("thread_spawn", otel.name = "thread_spawn");
            if let Some(trace) = parent_trace.as_ref() {
                let _ = set_parent_from_w3c_trace_context(&thread_spawn_span, trace);
            }
            Self::spawn_internal(SessionSpawnArgs {
                parent_trace,
                ..args
            })
            .instrument(thread_spawn_span)
            .await
        })
    }

    async fn spawn_internal(args: SessionSpawnArgs) -> CodexResult<(Arc<Self>, SessionIo)> {
        let SessionSpawnArgs {
            mut config,
            allow_provider_model_fallback,
            user_instructions,
            installation_id,
            auth_manager,
            models_manager,
            git_root_discovery,
            environment_manager,
            skills_service,
            plugins_manager,
            mcp_manager,
            code_mode_session_provider,
            extensions,
            conversation_history,
            requested_history_mode,
            fork_persistence,
            session_source,
            forked_from_thread_id,
            parent_thread_id,
            thread_source,
            originator,
            agent_control,
            dynamic_tools,
            metrics_service_name,
            user_shell_override,
            inherited_exec_policy,
            inherited_environments,
            parent_rollout_thread_trace,
            parent_trace: _,
            environment_selections,
            thread_extension_init,
            client_mcp_extensions,
            reserved_thread_id,
            analytics_events_client,
            thread_store,
            attestation_provider,
            external_time_provider,
            inherited_multi_agent_version,
            git_enrichment_policy,
            windows_sandbox_proxy_settings_mode,
        } = args;
        let (tx_sub, rx_sub) = async_channel::bounded(SUBMISSION_CHANNEL_CAPACITY);
        let (tx_event, rx_event) = async_channel::unbounded();

        let LoadedUserInstructions {
            instructions: user_instructions,
            warnings: user_instruction_provider_warnings,
        } = user_instructions;
        // TODO(anp) pull startup_warnings out of Config
        config
            .startup_warnings
            .extend(user_instruction_provider_warnings);
        let exec_policy = if crate::guardian::is_basic_session_source(&session_source) {
            let managed_policy = config
                .config_layer_stack
                .requirements()
                .exec_policy
                .as_deref()
                .map_or_else(codex_execpolicy::Policy::empty, |policy| {
                    policy.as_ref().clone()
                });
            Arc::new(ExecPolicyManager::new(Arc::new(managed_policy)))
        } else if matches!(
            &session_source,
            SessionSource::SubAgent(SubAgentSource::Compact)
        ) {
            // Compact handoff helpers should rely on built-in shell safety checks, not on
            // caller-provided rules that could shape the decoder or silently auto-approve work.
            Arc::new(ExecPolicyManager::default())
        } else if let Some(exec_policy) = &inherited_exec_policy {
            Arc::clone(exec_policy)
        } else {
            if !config
                .config_layer_stack
                .ignore_user_and_project_exec_policy_rules()
            {
                let codex_home = config.codex_home.clone();
                let policy_path = default_policy_path(codex_home.as_path());
                if let Err(err) = prefix_rule_migration(
                    codex_home.as_path(),
                    policy_path.as_path(),
                    BANNED_PREFIX_SUGGESTIONS,
                )
                .await
                {
                    tracing::warn!(error = %err, "failed to run prefix rule migration");
                }
            }
            Arc::new(
                ExecPolicyManager::load(&config.config_layer_stack)
                    .await
                    .map_err(|err| CodexErr::Fatal(format!("failed to load rules: {err}")))?,
            )
        };

        let mut config = Arc::new(config);
        let refresh_strategy = if session_source.is_non_root_agent() {
            codex_models_manager::manager::RefreshStrategy::Offline
        } else {
            codex_models_manager::manager::RefreshStrategy::OnlineIfUncached
        };
        if config.model.is_none()
            || !matches!(
                refresh_strategy,
                codex_models_manager::manager::RefreshStrategy::Offline
            )
        {
            let _ = models_manager
                .list_models(refresh_strategy, config.http_client_factory())
                .await;
        }
        let model = models_manager
            .get_default_model(
                &config.model,
                allow_provider_model_fallback,
                refresh_strategy,
                config.http_client_factory(),
            )
            .await;
        let trusted_guardian_reviewer = crate::guardian::is_basic_session_source(&session_source)
            && !matches!(conversation_history, InitialHistory::Resumed(_));
        if config
            .config_layer_stack
            .requirements()
            .auto_review_required_for_model(&model)
            && !trusted_guardian_reviewer
        {
            let config = Arc::make_mut(&mut config);
            if matches!(
                config.legacy_sandbox_policy(),
                SandboxPolicy::DangerFullAccess
            ) {
                let permission_profile = PermissionProfile::workspace_write();
                config
                    .permissions
                    .set_permission_profile(permission_profile.clone())
                    .map_err(|err| CodexErr::InvalidRequest(err.to_string()))?;
                if let Some(network) = config.permissions.network.as_ref() {
                    config.permissions.network = Some(
                        network
                            .recompute_for_permission_profile(&permission_profile)
                            .map_err(|err| CodexErr::InvalidRequest(err.to_string()))?,
                    );
                }
            }
            config
                .config_layer_stack
                .requirements()
                .approvals_reviewer
                .can_set(&ApprovalsReviewer::AutoReview)
                .map_err(|err| CodexErr::InvalidRequest(err.to_string()))?;
            config.approvals_reviewer = ApprovalsReviewer::AutoReview;
        }
        if allow_provider_model_fallback
            && let Some(requested_model) = config.model.as_ref()
            && model != *requested_model
        {
            info!(
                model_provider = %config.model_provider_id,
                requested_model,
                fallback_model = %model,
                "replaced unavailable requested model with provider default"
            );
        }

        // Resolve base instructions for the session. Priority order:
        // 1. config.base_instructions override
        // 2. conversation history => session_meta.base_instructions
        // 3. rendered instructions_template for current model
        let model_info = models_manager
            .get_model_info(model.as_str(), &config.to_models_manager_config())
            .await;
        let auth = auth_manager.auth_cached();
        token_budget::apply_experimental_context(Arc::make_mut(&mut config), auth.as_ref())?;
        // Intentionally resolve `enabled` and `use_history_notes_extension` only at
        // thread startup. Both activation flags stay fixed for this thread runtime,
        // even if the selected model changes later.
        token_budget::apply_model_defaults(Arc::make_mut(&mut config), &model_info);
        let configured_config = Arc::clone(&config);
        let multi_agent_version = config.multi_agent_version_override().or_else(|| {
            resolve_multi_agent_version(&conversation_history, inherited_multi_agent_version)
        });
        let history_mode = conversation_history.get_history_mode(
            requested_history_mode.unwrap_or_else(|| thread_store.default_history_mode()),
        );
        let base_instructions = config
            .base_instructions
            .clone()
            .or_else(|| conversation_history.get_base_instructions().map(|s| s.text))
            .unwrap_or_else(|| model_info.get_model_instructions(config.personality));

        // Dynamic tools are defined at thread start and persisted in rollout session metadata.
        let dynamic_tools = if dynamic_tools.is_empty() {
            conversation_history.get_dynamic_tools().unwrap_or_default()
        } else {
            dynamic_tools
        };
        // TODO (aibrahim): Consolidate config.model and config.model_reasoning_effort into config.collaboration_mode
        // to avoid extracting these fields separately and constructing CollaborationMode here.
        let collaboration_mode = CollaborationMode {
            mode: ModeKind::Default,
            settings: Settings {
                model: model.clone(),
                reasoning_effort: config.model_reasoning_effort.clone(),
                developer_instructions: None,
            },
        };
        let fast_mode_enabled = config.features.enabled(Feature::FastMode);
        let initial_service_tier_warning = unsupported_service_tier_warning(
            config.service_tier.as_deref(),
            fast_mode_enabled,
            &model_info,
        );
        let service_tier =
            get_service_tier(config.service_tier.clone(), fast_mode_enabled, &model_info);
        let session_configuration = SessionConfiguration {
            provider: create_model_provider(
                config.model_provider.clone(),
                Some(Arc::clone(&auth_manager)),
            ),
            step_settings: Arc::new(StepSettings {
                collaboration_mode,
                reasoning_summary: config.model_reasoning_summary,
                service_tier,
                personality: config.personality,
                approval_policy: config.permissions.approval_policy.clone(),
                approvals_reviewer: config.approvals_reviewer,
            }),
            model_info_overrides: config.to_models_manager_config().into(),
            developer_instructions: config.developer_instructions.clone(),
            base_instructions,
            permission_profile_state: session_permission_profile_state_from_config(&config)?,
            allow_login_shell: config.permissions.allow_login_shell,
            shell_environment_policy: config.permissions.shell_environment_policy.clone(),
            windows_sandbox_level: WindowsSandboxLevel::from_config(&config),
            windows_sandbox_private_desktop: config.permissions.windows_sandbox_private_desktop,
            use_legacy_landlock: config.features.use_legacy_landlock(),
            legacy_fallback_cwd: config.cwd.clone(),
            codex_home: config.codex_home.clone(),
            thread_name: None,
            original_config_do_not_use: Arc::clone(&config),
            metrics_service_name,
            app_server_client_name: None,
            app_server_client_version: None,
            trusted_guardian_reviewer,
            session_source,
            history_mode,
            forked_from_thread_id,
            parent_thread_id,
            thread_source,
            originator,
            dynamic_tools,
            user_shell_override,
        };
        session_configuration
            .validate(&environment_selections)
            .map_err(|err| CodexErr::InvalidRequest(err.to_string()))?;

        // Generate a unique ID for the lifetime of this session.
        let session_source_clone = session_configuration.session_source.clone();
        let (agent_status_tx, agent_status_rx) = watch::channel(AgentStatus::PendingInit);

        let session = Box::pin(Session::new(
            session_configuration,
            &environment_selections,
            config.clone(),
            user_instructions,
            installation_id,
            auth_manager.clone(),
            models_manager.clone(),
            git_root_discovery,
            model_info,
            exec_policy,
            tx_event.clone(),
            agent_status_tx.clone(),
            conversation_history,
            fork_persistence,
            session_source_clone,
            skills_service,
            plugins_manager,
            mcp_manager.clone(),
            code_mode_session_provider,
            extensions,
            thread_extension_init,
            client_mcp_extensions,
            agent_control,
            reserved_thread_id,
            environment_manager,
            inherited_environments,
            analytics_events_client,
            thread_store,
            parent_rollout_thread_trace,
            attestation_provider,
            external_time_provider,
            multi_agent_version,
            git_enrichment_policy,
            windows_sandbox_proxy_settings_mode,
        ))
        .await
        .map_err(|e| {
            error!("Failed to create session: {e:#}");
            map_session_init_error(&e, &config.codex_home)
        })?;
        if let Some(message) = initial_service_tier_warning {
            session
                .send_event_raw(Event {
                    id: INITIAL_SUBMIT_ID.to_owned(),
                    msg: EventMsg::Warning(WarningEvent { message }),
                })
                .await;
        }
        let thread_id = session.thread_id;

        // This task will run until Op::Shutdown is received.
        let session_for_loop = Arc::clone(&session);
        let session_loop_handle = tokio::spawn(async move {
            submission_loop(session_for_loop, configured_config, rx_sub)
                .instrument(info_span!("session_loop", thread_id = %thread_id))
                .await;
        });
        let io = SessionIo {
            tx_sub,
            rx_event,
            submission_admission: Arc::clone(&session.submission_admission),
            agent_status: agent_status_rx,
            session_loop_termination: session_loop_termination_from_handle(session_loop_handle),
        };

        Ok((session, io))
    }
}

impl SessionIo {
    /// Submit the `op` wrapped in a `Submission` with a unique ID.
    pub(crate) async fn submit(&self, op: Op) -> CodexResult<String> {
        self.submit_with_trace(
            op, /*trace*/ None, /*parent_turn_id*/ None, /*root_turn_id*/ None,
        )
        .await
    }

    pub(crate) async fn submit_with_trace(
        &self,
        op: Op,
        trace: Option<W3cTraceContext>,
        parent_turn_id: Option<String>,
        root_turn_id: Option<String>,
    ) -> CodexResult<String> {
        let id = new_submission_id();
        let sub = Submission {
            id: id.clone(),
            op,
            trace,
            parent_turn_id,
            root_turn_id,
        };
        self.submit_with_id(sub).await?;
        Ok(id)
    }

    pub(crate) async fn submit_accepted_completion(&self, op: Op) -> CodexResult<String> {
        debug_assert!(matches!(op, Op::InterAgentCommunication { .. }));
        let id = new_submission_id();
        let sub = Submission {
            id: id.clone(),
            op,
            trace: None,
            parent_turn_id: None,
            root_turn_id: None,
        };
        self.submit_with_id_and_admission(sub, SubmissionAdmissionKind::AcceptedCompletion)
            .await?;
        Ok(id)
    }

    pub(crate) async fn submit_turn_input_with_admission(
        &self,
        session: &Session,
        mut request: TurnInputRequest,
        mode: TurnInputMode,
        policy: InputTurnAdmissionPolicy,
        agent_queue_turn: Option<codex_protocol::protocol::AgentQueueTurnMetadata>,
    ) -> CodexResult<SubmittedInputTurn> {
        let id = new_submission_id();
        let mut admission =
            session.register_input_turn_admission(id.clone(), policy, agent_queue_turn);
        let (reply_tx, reply_rx) = oneshot::channel();
        let trace = request.trace.take();
        self.submit_with_id(Submission {
            id: id.clone(),
            op: Op::TurnInput {
                request: Box::new(request),
                mode,
                reply: reply_tx,
            },
            trace,
            parent_turn_id: None,
            root_turn_id: None,
        })
        .await?;
        admission.mark_submitted();
        let _ = reply_rx.await.unwrap_or(Err(CodexErr::InternalAgentDied))?;
        let outcome = tokio::select! {
            admission = admission.recv() => {
                admission.ok_or(CodexErr::InternalAgentDied)??
            },
            () = self.session_loop_termination.clone() => {
                return Err(CodexErr::InternalAgentDied);
            }
        };
        Ok(outcome)
    }

    /// Use sparingly: prefer `submit()` so submission IDs are generated consistently.
    pub(crate) async fn submit_with_id(&self, sub: Submission) -> CodexResult<()> {
        self.submit_with_id_and_admission(sub, SubmissionAdmissionKind::Ordinary)
            .await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "the send lock must preserve admission checks and bounded-channel queue ordering"
    )]
    async fn submit_with_id_and_admission(
        &self,
        mut sub: Submission,
        submission_admission: SubmissionAdmissionKind,
    ) -> CodexResult<()> {
        let is_shutdown = matches!(&sub.op, Op::Shutdown);
        let is_rollback = matches!(
            &sub.op,
            Op::ThreadRollback { .. } | Op::ThreadRollbackMaterialized { .. }
        );
        let is_completion_submission = matches!(
            submission_admission,
            SubmissionAdmissionKind::AcceptedCompletion
        ) || matches!(
            &sub.op,
            Op::InterAgentCommunication { communication, .. }
                if communication.id.as_ref().is_some_and(|id| {
                    is_sub_agent_completion_context_response_item_id(id.as_str())
                })
        );
        let _shutdown_sequence_guard = if is_shutdown {
            Some(
                self.submission_admission
                    .shutdown_sequence_lock
                    .lock()
                    .await,
            )
        } else {
            None
        };
        let shutdown_was_pending = self
            .submission_admission
            .shutdown_pending
            .load(std::sync::atomic::Ordering::Acquire);
        let armed_shutdown = is_shutdown && !shutdown_was_pending;
        let mut completion_shutdown_guard = armed_shutdown
            .then(|| CompletionShutdownAdmissionGuard::new(Arc::clone(&self.submission_admission)));
        let mut shutdown_guard = armed_shutdown
            .then(|| ShutdownAdmissionGuard::new(Arc::clone(&self.submission_admission)));
        if armed_shutdown {
            self.submission_admission
                .shutdown_pending
                .store(true, std::sync::atomic::Ordering::Release);
            self.submission_admission.changed.notify_waiters();
            self.submission_admission
                .wait_for_accepted_completion_deliveries()
                .await;
        }
        // Serialize channel sends so their queue order cannot cross a rollback reservation. The
        // state lock is released before awaiting bounded-channel capacity, allowing the rollback
        // handler to complete or quarantine the session without deadlocking on a blocked sender.
        let _send_guard = self.submission_admission.send_lock.lock().await;
        let shutdown_is_pending = self
            .submission_admission
            .shutdown_pending
            .load(std::sync::atomic::Ordering::Acquire);
        if shutdown_is_pending
            && !is_shutdown
            && matches!(submission_admission, SubmissionAdmissionKind::Ordinary)
        {
            return Err(CodexErr::InvalidRequest(
                "thread shutdown is already in progress".to_string(),
            ));
        }
        let mut rollback_guard = {
            let mut state = self
                .submission_admission
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match *state {
                SubmissionAdmissionState::Ready => {}
                SubmissionAdmissionState::RollbackEventPending
                    if !is_shutdown && !is_rollback && !is_completion_submission => {}
                SubmissionAdmissionState::RollbackPending
                | SubmissionAdmissionState::RollbackEventPending
                    if !is_shutdown =>
                {
                    return Err(CodexErr::InvalidRequest(
                        "thread rollback is already in progress".to_string(),
                    ));
                }
                SubmissionAdmissionState::ReloadRequired if !is_shutdown => {
                    return Err(CodexErr::InvalidRequest(
                        "thread history must be reloaded before accepting more work".to_string(),
                    ));
                }
                SubmissionAdmissionState::RollbackPending
                | SubmissionAdmissionState::RollbackEventPending
                | SubmissionAdmissionState::ReloadRequired => {}
            }
            if is_rollback {
                *state = SubmissionAdmissionState::RollbackPending;
                Some(RollbackAdmissionGuard::new(Arc::clone(
                    &self.submission_admission,
                )))
            } else {
                None
            }
        };
        if sub.trace.is_none() {
            sub.trace = current_span_w3c_trace_context();
        }
        match self.tx_sub.send(sub).await {
            Ok(()) => {
                if let Some(rollback_guard) = rollback_guard.as_mut() {
                    rollback_guard.disarm();
                }
                if let Some(shutdown_guard) = shutdown_guard.as_mut() {
                    shutdown_guard.disarm();
                }
                if let Some(completion_shutdown_guard) = completion_shutdown_guard.as_mut() {
                    completion_shutdown_guard.disarm();
                }
                Ok(())
            }
            Err(_) => Err(CodexErr::InternalAgentDied),
        }
    }

    /// Submits an ordered turn-input call and waits only for Core's routing decision.
    ///
    /// Once queued, dropping the waiter does not retract the call. If the
    /// session loop exits before replying, the caller gets `InternalAgentDied`.
    pub(crate) async fn submit_turn_input(
        &self,
        mut request: TurnInputRequest,
        mode: TurnInputMode,
    ) -> CodexResult<TurnInputSubmission> {
        let id = new_submission_id();
        let (reply_tx, reply_rx) = oneshot::channel();
        let trace = request.trace.take();
        self.submit_with_id(Submission {
            id,
            op: Op::TurnInput {
                request: Box::new(request),
                mode,
                reply: reply_tx,
            },
            trace,
            parent_turn_id: None,
            root_turn_id: None,
        })
        .await?;
        reply_rx.await.unwrap_or(Err(CodexErr::InternalAgentDied))
    }

    pub(crate) async fn start_turn_if_idle_with_lease(
        &self,
        session: &Arc<Session>,
        request: TurnInputRequest,
        reservation_lease: impl Send,
    ) -> CodexResult<TurnInputSubmission> {
        turn_input::start_if_idle_with_lease(
            session,
            request,
            new_submission_id(),
            turn_input::TurnStartKind::Automatic,
            reservation_lease,
        )
        .await
    }

    pub(crate) async fn submit_recover_turn(
        &self,
        thread_settings: ThreadSettingsOverrides,
        start_options: TurnStartOptions,
        trace: Option<W3cTraceContext>,
        turn_id: String,
    ) -> CodexResult<TurnInputSubmission> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.submit_with_id(Submission {
            id: turn_id,
            op: Op::RecoverTurn {
                thread_settings,
                start_options,
                reply: reply_tx,
            },
            trace,
            parent_turn_id: None,
            root_turn_id: None,
        })
        .await?;
        reply_rx.await.unwrap_or(Err(CodexErr::InternalAgentDied))
    }

    pub(crate) async fn shutdown_and_wait(&self) -> CodexResult<()> {
        let session_loop_termination = self.session_loop_termination.clone();
        match self.submit(Op::Shutdown).await {
            Ok(_) => {}
            Err(err) if matches!(err.details(), CodexErrorDetails::InternalAgentDied) => {}
            Err(err) => return Err(err),
        }
        session_loop_termination.await;
        Ok(())
    }

    pub(crate) async fn next_event(&self) -> CodexResult<Event> {
        let event = self
            .rx_event
            .recv()
            .await
            .map_err(|_| CodexErr::InternalAgentDied)?;
        Ok(event)
    }

    pub(crate) async fn agent_status(&self) -> AgentStatus {
        self.agent_status.borrow().clone()
    }
}

/// Generate a core submission ID. App-server exposes submission IDs that
/// create turns as a public-facing turn ID.
///
/// Some use cases take advantage of the fact that these are UUID7 which
/// encodes a timestamp, so think carefully before changing this.
pub(crate) fn new_submission_id() -> String {
    Uuid::now_v7().to_string()
}

fn get_service_tier(
    configured_service_tier: Option<String>,
    fast_mode_enabled: bool,
    model_info: &ModelInfo,
) -> Option<String> {
    if !fast_mode_enabled {
        return None;
    }
    configured_service_tier.filter(|service_tier| {
        service_tier == SERVICE_TIER_DEFAULT_REQUEST_VALUE
            || model_info.supports_service_tier(service_tier)
    })
}

fn unsupported_service_tier_warning(
    configured_service_tier: Option<&str>,
    fast_mode_enabled: bool,
    model_info: &ModelInfo,
) -> Option<String> {
    let service_tier = configured_service_tier.filter(|service_tier| {
        fast_mode_enabled
            && *service_tier != SERVICE_TIER_DEFAULT_REQUEST_VALUE
            && !model_info.supports_service_tier(service_tier)
    })?;
    Some(format!(
        "Configured service tier `{service_tier}` is not advertised as supported for model `{}` and will be omitted from requests.",
        model_info.slug
    ))
}

fn session_permission_profile_state_from_config(
    config: &Config,
) -> CodexResult<PermissionProfileState> {
    Ok(config.permissions.permission_profile_state().clone())
}

#[cfg(test)]
pub(crate) fn completed_session_loop_termination() -> SessionLoopTermination {
    futures::future::ready(()).boxed().shared()
}

pub(crate) fn session_loop_termination_from_handle(
    handle: JoinHandle<()>,
) -> SessionLoopTermination {
    async move {
        let _ = handle.await;
    }
    .boxed()
    .shared()
}

async fn thread_title_from_thread_store(
    live_thread: Option<&LiveThread>,
    thread_store: &Arc<dyn ThreadStore>,
    conversation_id: ThreadId,
) -> Option<String> {
    let thread = match live_thread {
        Some(live_thread) => {
            live_thread
                .read_thread(
                    /*include_archived*/ true, /*include_history*/ false,
                )
                .await
        }
        None => {
            thread_store
                .read_thread(ReadThreadParams {
                    thread_id: conversation_id,
                    include_archived: true,
                    include_history: false,
                })
                .await
        }
    }
    .ok()?;

    let title = thread.name.as_deref()?.trim();
    (!title.is_empty() && thread.preview.trim() != title).then(|| title.to_string())
}

impl Session {
    pub(crate) async fn app_server_client_metadata(&self) -> AppServerClientMetadata {
        let state = self.state.lock().await;
        AppServerClientMetadata {
            client_name: state.session_configuration.app_server_client_name.clone(),
            client_version: state
                .session_configuration
                .app_server_client_version
                .clone(),
        }
    }

    fn managed_network_proxy_active_for_permission_profile(
        permission_profile: &PermissionProfile,
    ) -> bool {
        !matches!(permission_profile, PermissionProfile::Disabled)
    }

    /// Builds the `x-codex-beta-features` header value for this session.
    ///
    /// `ModelClient` is session-scoped and intentionally does not depend on the full `Config`, so
    /// we precompute the comma-separated list of enabled experimental feature keys at session
    /// creation time and thread it into the client.
    fn build_model_client_beta_features_header(config: &Config) -> Option<String> {
        let beta_features_header = FEATURES
            .iter()
            .filter_map(|spec| {
                let advertise_in_model_client_header =
                    spec.stage.experimental_menu_description().is_some()
                        || spec.id == Feature::RemoteCompactionV2;
                if advertise_in_model_client_header && config.features.enabled(spec.id) {
                    Some(spec.key)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(",");

        if beta_features_header.is_empty() {
            None
        } else {
            Some(beta_features_header)
        }
    }

    async fn start_managed_network_proxy(
        spec: &crate::config::NetworkProxySpec,
        exec_policy: &codex_execpolicy::Policy,
        permission_profile: &PermissionProfile,
        network_policy_decider: Option<Arc<dyn codex_network_proxy::NetworkPolicyDecider>>,
        blocked_request_observer: Option<Arc<dyn codex_network_proxy::BlockedRequestObserver>>,
        managed_network_requirements_enabled: bool,
        audit_metadata: NetworkProxyAuditMetadata,
    ) -> anyhow::Result<(StartedNetworkProxy, SessionNetworkProxyRuntime)> {
        let spec = spec
            .with_exec_policy_network_rules(exec_policy)
            .map_err(|err| {
                tracing::warn!(
                    "failed to apply execpolicy network rules to managed proxy; continuing with configured network policy: {err}"
                );
                err
            })
            .unwrap_or_else(|_| spec.clone());
        let network_proxy = spec
            .start_proxy(
                permission_profile,
                network_policy_decider,
                blocked_request_observer,
                managed_network_requirements_enabled,
                audit_metadata,
            )
            .await
            .map_err(|err| anyhow::anyhow!("failed to start managed network proxy: {err}"))?;
        let session_network_proxy = {
            let proxy = network_proxy.proxy();
            SessionNetworkProxyRuntime {
                http_addr: proxy.http_addr().to_string(),
                socks_addr: proxy.socks_addr().to_string(),
            }
        };
        Ok((network_proxy, session_network_proxy))
    }

    async fn refresh_managed_network_proxy_for_current_permission_profile(&self) {
        let Ok(_refresh_guard) = self.managed_network_proxy_refresh_lock.acquire().await else {
            error!("managed network proxy refresh semaphore closed");
            return;
        };
        let session_configuration = {
            let state = self.state.lock().await;
            state.session_configuration.clone()
        };
        let Some(spec) = session_configuration
            .original_config_do_not_use
            .permissions
            .network
            .as_ref()
            .cloned()
        else {
            self.services.network_proxy.store(None);
            return;
        };

        let spec = match spec
            .recompute_for_permission_profile(&session_configuration.permission_profile())
        {
            Ok(spec) => spec,
            Err(err) => {
                warn!("failed to rebuild managed network proxy policy for sandbox change: {err}");
                return;
            }
        };
        let current_exec_policy = self.services.exec_policy.current();
        let spec = match spec.with_exec_policy_network_rules(current_exec_policy.as_ref()) {
            Ok(spec) => spec,
            Err(err) => {
                warn!(
                    "failed to apply execpolicy network rules while refreshing managed network proxy: {err}"
                );
                spec
            }
        };
        // Disabled specs still carry managed requirements and constraints, but they do not have
        // listeners and must not be exposed as active managed proxy runtimes.
        if !spec.enabled() {
            self.services.network_proxy.store(None);
            return;
        }
        if let Some(started_proxy) = self.services.network_proxy.load_full() {
            if let Err(err) = spec.apply_to_started_proxy(started_proxy.as_ref()).await {
                warn!("failed to refresh managed network proxy for sandbox change: {err}");
            }
            return;
        }

        match Self::start_managed_network_proxy(
            &spec,
            current_exec_policy.as_ref(),
            &session_configuration.permission_profile(),
            /*network_policy_decider*/ None,
            Some(build_blocked_request_observer(Arc::clone(
                &self.services.network_approval,
            ))),
            self.services.managed_network_requirements_configured,
            self.services.network_proxy_audit_metadata.clone(),
        )
        .await
        {
            Ok((started_proxy, _session_network_proxy)) => {
                self.services
                    .network_proxy
                    .store(Some(Arc::new(started_proxy)));
            }
            Err(err) => {
                warn!("failed to start managed network proxy for sandbox change: {err}");
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn codex_home(&self) -> AbsolutePathBuf {
        let state = self.state.lock().await;
        state.session_configuration.codex_home().clone()
    }

    pub(crate) fn subscribe_elicitation_pause_state(&self) -> watch::Receiver<bool> {
        self.services.elicitations.subscribe()
    }

    pub(crate) fn mark_interrupted(&self) {
        self.agent_status.send_replace(AgentStatus::Interrupted);
    }

    pub(crate) fn is_interrupted(&self) -> bool {
        matches!(*self.agent_status.borrow(), AgentStatus::Interrupted)
    }

    pub(crate) fn get_tx_event(&self) -> Sender<Event> {
        self.tx_event.clone()
    }

    pub(crate) fn state_db(&self) -> Option<state_db::StateDbHandle> {
        self.services.state_db.clone()
    }

    pub(crate) fn live_thread_for_persistence(
        &self,
        operation: &str,
    ) -> anyhow::Result<&LiveThread> {
        self.live_thread()
            .ok_or_else(|| anyhow::anyhow!("Session persistence is disabled; cannot {operation}."))
    }

    pub(crate) fn live_thread(&self) -> Option<&LiveThread> {
        self.services.live_thread.as_ref()
    }

    pub(crate) async fn set_thread_memory_mode(
        self: &Arc<Self>,
        mode: ThreadMemoryMode,
    ) -> anyhow::Result<()> {
        handlers::persist_thread_memory_mode_update(self, mode).await
    }

    /// Flush rollout writes and return the final durability-barrier result.
    #[instrument(name = "session.flush_rollout", level = "trace", skip_all)]
    pub(crate) async fn flush_rollout(&self) -> std::io::Result<()> {
        if let Some(live_thread) = self.live_thread() {
            live_thread.flush().await.map_err(std::io::Error::other)
        } else {
            Ok(())
        }
    }

    pub(crate) async fn try_ensure_rollout_materialized(
        &self,
        context: PersistContext,
    ) -> std::io::Result<()> {
        if let Some(live_thread) = self.live_thread() {
            live_thread
                .persist(context)
                .await
                .map_err(std::io::Error::other)?;
        }
        Ok(())
    }

    pub(crate) async fn ensure_rollout_materialized(&self, context: PersistContext) {
        if let Err(e) = self.try_ensure_rollout_materialized(context).await {
            warn!("failed to materialize thread persistence: {e}");
        }
    }

    fn next_internal_sub_id(&self) -> String {
        let id = self
            .next_internal_sub_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        format!("auto-compact-{id}")
    }

    pub(crate) async fn get_total_token_usage(&self) -> i64 {
        let state = self.state.lock().await;
        state.get_total_token_usage(state.server_reasoning_included())
    }

    pub(crate) async fn auto_compact_window_snapshot(&self) -> AutoCompactWindowSnapshot {
        let state = self.state.lock().await;
        state.auto_compact_window_snapshot()
    }

    pub(crate) async fn estimated_tokens_after_last_model_generated_item(&self) -> i64 {
        let state = self.state.lock().await;
        state
            .history
            .estimated_tokens_after_last_model_generated_item()
    }

    pub(crate) async fn total_token_usage(&self) -> Option<TokenUsage> {
        let state = self.state.lock().await;
        state.token_info().map(|info| info.total_token_usage)
    }

    /// Returns the complete token usage snapshot currently cached for this session.
    ///
    /// Resume and fork reconstruction seed this state from the last persisted rollout
    /// `TokenCount` event. Callers that need to replay restored usage to a client
    /// should use this accessor instead of `total_token_usage`, because the app-server
    /// notification includes both total and last-turn usage.
    pub(crate) async fn token_usage_info(&self) -> Option<TokenUsageInfo> {
        let state = self.state.lock().await;
        state.token_info()
    }

    pub(crate) async fn get_estimated_token_count(
        &self,
        turn_context: &TurnContext,
    ) -> Option<i64> {
        let history = self.clone_history().await;
        history.estimate_token_count(turn_context)
    }

    pub(crate) async fn get_base_instructions(&self) -> BaseInstructions {
        let state = self.state.lock().await;
        BaseInstructions {
            text: state.session_configuration.base_instructions.clone(),
            provenance: state.base_instructions_provenance.clone(),
        }
    }

    /// Render the request copy without changing instructions persisted or inherited by forks.
    pub(crate) async fn get_prompt_base_instructions(&self) -> BaseInstructions {
        let config = self.get_config().await;
        let instructions = self.get_base_instructions().await;
        if !config.update_plan_enabled
            && config.model_catalog.is_none()
            && matches!(
                instructions.provenance,
                Some(BaseInstructionsProvenance::Model { .. })
            )
        {
            BaseInstructions {
                text: crate::context::without_update_plan_instructions(&instructions.text),
                ..instructions
            }
        } else {
            instructions
        }
    }

    // Merges connector IDs into the session-level explicit connector selection.
    #[tracing::instrument(
        level = "trace",
        skip_all,
        fields(connector_count = connector_ids.len())
    )]
    pub(crate) async fn merge_connector_selection(
        &self,
        connector_ids: HashSet<String>,
    ) -> HashSet<String> {
        let mut state = self.state.lock().await;
        state.merge_connector_selection(connector_ids)
    }

    // Returns the connector IDs currently selected for this session.
    pub(crate) async fn get_connector_selection(&self) -> HashSet<String> {
        let state = self.state.lock().await;
        state.get_connector_selection()
    }

    // Clears connector IDs that were accumulated for explicit selection.
    pub(crate) async fn clear_connector_selection(&self) {
        let mut state = self.state.lock().await;
        state.clear_connector_selection();
    }

    pub(crate) async fn latest_mcp_server_use_context_text(
        &self,
        server_name: &str,
    ) -> Option<String> {
        {
            let queued = self.input_queue.queued_response_items_for_next_turn().await;
            if let Some(text) = queued.iter().rev().find_map(|item| {
                let ResponseItem::Message { role, content, .. } = item else {
                    return None;
                };
                if role != "developer" {
                    return None;
                }
                content.iter().find_map(|content_item| match content_item {
                    ContentItem::InputText { text }
                        if crate::context::McpServerUseInstructions::parse_server_name(text)
                            .as_deref()
                            == Some(server_name) =>
                    {
                        Some(text.clone())
                    }
                    ContentItem::InputText { .. }
                    | ContentItem::InputImage { .. }
                    | ContentItem::InputAudio { .. }
                    | ContentItem::OutputText { .. } => None,
                })
            }) {
                return Some(text);
            }
        }
        let active_turn_state = {
            let active = self.active_turn.lock().await;
            active
                .as_ref()
                .map(|active_turn| Arc::clone(&active_turn.turn_state))
        };
        if let Some(active_turn_state) = active_turn_state {
            let turn_state = active_turn_state.lock().await;
            if let Some(text) = turn_state.pending_input().iter().rev().find_map(|item| {
                let TurnInput::ResponseItem(response) = item else {
                    return None;
                };
                let ResponseItem::Message { role, content, .. } = &response.item else {
                    return None;
                };
                if role != "developer" {
                    return None;
                }
                content.iter().find_map(|content_item| match content_item {
                    ContentItem::InputText { text }
                        if crate::context::McpServerUseInstructions::parse_server_name(text)
                            .as_deref()
                            == Some(server_name) =>
                    {
                        Some(text.clone())
                    }
                    ContentItem::InputText { .. }
                    | ContentItem::InputImage { .. }
                    | ContentItem::InputAudio { .. }
                    | ContentItem::OutputText { .. } => None,
                })
            }) {
                return Some(text);
            }
        }
        let history = self.clone_history().await;
        history.raw_items().rev().find_map(|item| {
            let ResponseItem::Message { role, content, .. } = item else {
                return None;
            };
            if role != "developer" {
                return None;
            }
            content.iter().find_map(|content_item| match content_item {
                ContentItem::InputText { text }
                    if crate::context::McpServerUseInstructions::parse_server_name(text)
                        .as_deref()
                        == Some(server_name) =>
                {
                    Some(text.clone())
                }
                ContentItem::InputText { .. }
                | ContentItem::InputImage { .. }
                | ContentItem::InputAudio { .. }
                | ContentItem::OutputText { .. } => None,
            })
        })
    }

    pub(crate) async fn render_mcp_server_use_context_text(&self, server_name: &str) -> String {
        let tool_inventory_json = {
            let mut tool_inventory = self
                .services
                .mcp_runtime
                .current_binding_for_call(server_name)
                .await
                .map(|binding| binding.tools().to_vec())
                .unwrap_or_default()
                .into_iter()
                .filter(|tool| tool.server_name == server_name)
                .map(|tool| {
                    let canonical_tool_name = tool.canonical_tool_name().to_string();
                    let inventory_entry = serde_json::json!({
                            "canonical_tool_name": &canonical_tool_name,
                            "tool_info": tool,
                    });
                    (canonical_tool_name, inventory_entry)
                })
                .collect::<Vec<_>>();
            tool_inventory.sort_by(|(left, _), (right, _)| left.cmp(right));
            let tool_inventory = tool_inventory
                .into_iter()
                .map(|(_, tool)| tool)
                .collect::<Vec<_>>();
            if tool_inventory.is_empty() {
                "[]".to_string()
            } else {
                match serde_json::to_string_pretty(&tool_inventory) {
                    Ok(tool_inventory_json) => tool_inventory_json,
                    Err(error) => {
                        warn!(
                            "failed to serialize MCP tool inventory for `{server_name}`: {error}"
                        );
                        "[]".to_string()
                    }
                }
            }
        };
        crate::context::McpServerUseInstructions::new(server_name.to_string(), tool_inventory_json)
            .render()
    }

    pub(crate) fn mcp_server_was_implicitly_visible_at_session_start(
        &self,
        server_name: &str,
    ) -> bool {
        if !self
            .session_start_mcp_servers
            .get(server_name)
            .is_some_and(|server| server.enabled() && server.config().allow_implicit_invocation)
        {
            return false;
        }
        if let Some(direct_mcp_servers) = self
            .session_start_direct_mcp_servers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            return direct_mcp_servers.contains(server_name);
        }
        false
    }

    pub(crate) async fn current_mcp_inventory_was_direct_at_session_start(
        &self,
        server_name: &str,
    ) -> bool {
        if !self.mcp_server_was_implicitly_visible_at_session_start(server_name) {
            return false;
        }
        let frozen_direct_tools = {
            let frozen_direct_tools = self.session_start_direct_mcp_tools.lock().await;
            let Some(frozen_direct_tools) = frozen_direct_tools.as_ref() else {
                return false;
            };
            frozen_direct_tools.clone()
        };
        let current_tools = self
            .services
            .mcp_runtime
            .current_binding_for_call(server_name)
            .await
            .map(|binding| binding.tools().to_vec())
            .unwrap_or_default();
        let current_tools = current_tools
            .iter()
            .filter(|tool| tool.server_name == server_name)
            .map(|tool| (tool.canonical_tool_name().to_string(), tool))
            .collect::<HashMap<_, _>>();
        let frozen_direct_tools = frozen_direct_tools
            .iter()
            .filter(|(_, tool)| tool.server_name == server_name)
            .collect::<HashMap<_, _>>();
        current_tools.len() == frozen_direct_tools.len()
            && current_tools.iter().all(|(name, tool)| {
                frozen_direct_tools.get(name).is_some_and(|frozen_tool| {
                    serde_json::to_value(frozen_tool).ok() == serde_json::to_value(tool).ok()
                })
            })
    }

    pub(crate) async fn session_start_mcp_tools_for_exposure(
        &self,
    ) -> HashMap<String, McpToolInfo> {
        let direct_contract_unfrozen = {
            self.session_start_direct_mcp_servers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none()
        };
        if direct_contract_unfrozen {
            let ready_tools = self
                .collect_available_session_start_implicit_mcp_tools()
                .await;
            let mut session_start_mcp_tools = self.session_start_mcp_tools.lock().await;
            let direct_contract_still_unfrozen = {
                self.session_start_direct_mcp_servers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_none()
            };
            if direct_contract_still_unfrozen {
                *session_start_mcp_tools = ready_tools;
            }
            return session_start_mcp_tools.clone();
        }
        self.session_start_mcp_tools.lock().await.clone()
    }

    async fn collect_available_session_start_implicit_mcp_tools(
        &self,
    ) -> HashMap<String, McpToolInfo> {
        let available_tools = self
            .services
            .mcp_runtime
            .current_binding()
            .await
            .map(|binding| binding.tools().to_vec())
            .unwrap_or_default();
        available_tools
            .into_iter()
            .filter(|tool| {
                self.session_start_mcp_servers
                    .get(&tool.server_name)
                    .is_some_and(|server| {
                        server.enabled() && server.config().allow_implicit_invocation
                    })
            })
            .map(|tool| (tool.canonical_tool_name().to_string(), tool))
            .collect()
    }

    async fn record_initial_history(
        &self,
        conversation_history: InitialHistory,
    ) -> anyhow::Result<()> {
        let (is_subagent, is_paginated_subagent) = {
            let state = self.state.lock().await;
            let session_configuration = &state.session_configuration;
            (
                session_configuration.session_source.is_non_root_agent(),
                matches!(
                    session_configuration.history_mode,
                    ThreadHistoryMode::Paginated
                ) && matches!(
                    session_configuration.thread_source.as_ref(),
                    Some(ThreadSource::Subagent | ThreadSource::GuardianReview)
                ),
            )
        };
        let has_prior_user_turns = initial_history_has_prior_user_turns(&conversation_history);
        {
            let mut state = self.state.lock().await;
            state.set_next_turn_is_first(!has_prior_user_turns);
        }
        let turn_context = match conversation_history {
            InitialHistory::New | InitialHistory::Cleared => {
                // Defer initial context insertion until the first real turn starts so
                // turn/start overrides can be merged before we write model-visible context.
                self.set_previous_turn_settings(/*previous_turn_settings*/ None)
                    .await;
                None
            }
            InitialHistory::Resumed(resumed_history) => {
                let turn_context = self.new_default_turn().await;
                let rollout_items = resumed_history.history;
                if matches!(
                    rollout_items.iter().rev().find_map(|item| match item {
                        RolloutItem::EventMsg(event) => agent_status_from_event(event),
                        _ => None,
                    }),
                    Some(AgentStatus::Interrupted)
                ) {
                    self.agent_status.send_replace(AgentStatus::Interrupted);
                }
                let applied_reconstruction = self
                    .apply_rollout_reconstruction(&turn_context, &rollout_items)
                    .await;
                let previous_turn_settings = applied_reconstruction.previous_turn_settings;
                if let Some(repair) = applied_reconstruction.repair.as_ref() {
                    self.persist_reconstruction_repair_with_policy(repair)
                        .await?;
                }

                // If resuming, warn when the last recorded model differs from the current one.
                let curr: &str = turn_context.model_info().slug.as_str();
                if let Some(prev) = previous_turn_settings
                    .as_ref()
                    .map(|settings| settings.model.as_str())
                    .filter(|model| *model != curr)
                {
                    warn!("resuming session with different model: previous={prev}, current={curr}");
                    self.send_event(
                        &turn_context,
                        EventMsg::Warning(WarningEvent {
                            message: format!(
                                "This session was recorded with model `{prev}` but is resuming with `{curr}`. \
                         Consider switching back to `{prev}` as it may affect Codex performance."
                            ),
                        }),
                    )
                    .await;
                }

                // Seed usage info from the recorded rollout so UIs can show token counts
                // immediately on resume/fork.
                if let Some(info) = Self::last_token_info_from_rollout(&rollout_items) {
                    let mut state = self.state.lock().await;
                    state.set_token_info(Some(info));
                }
                self.state.lock().await.latest_token_usage_record =
                    Self::last_token_usage_record_from_rollout(&rollout_items);
                if applied_reconstruction.should_recompute_token_usage {
                    self.recompute_token_usage(&turn_context).await;
                }

                // Defer seeding the session's initial context until the first turn starts so
                // turn/start overrides can be merged before we write to the rollout.
                if !is_subagent {
                    let _ = self.flush_rollout().await;
                }
                None
            }
            InitialHistory::Forked(mut rollout_items) => {
                let turn_context = self.new_default_turn().await;
                Self::assign_missing_rollout_response_item_ids(&mut rollout_items);
                let applied_reconstruction = self
                    .apply_rollout_reconstruction(&turn_context, &rollout_items)
                    .await;

                // Seed usage info from the recorded rollout so UIs can show token counts
                // immediately on resume/fork.
                if let Some(info) = Self::last_token_info_from_rollout(&rollout_items) {
                    let mut state = self.state.lock().await;
                    state.set_token_info(Some(info));
                }
                self.state.lock().await.latest_token_usage_record =
                    Self::last_token_usage_record_from_rollout(&rollout_items);

                let thread_settings_applied =
                    RolloutItem::EventMsg(thread_settings::applied_event(self).await);
                match &self.fork_persistence {
                    ForkPersistence::Referenced {
                        inherited_item_count,
                        ..
                    } => {
                        // Ancestor records remain behind history_base; only effective child
                        // settings and boundaries synthesized by snapshot processing are local.
                        rollout_items.drain(..*inherited_item_count);
                        rollout_items.insert(0, thread_settings_applied);
                    }
                    ForkPersistence::Copied if is_paginated_subagent => {
                        // Paginated subagents already persist inherited context when their live
                        // thread is created.
                        rollout_items.clear();
                        rollout_items.push(thread_settings_applied);
                    }
                    ForkPersistence::Copied => {
                        // Keep the copied prefix and effective child settings in one append so a
                        // cold resume cannot observe inherited settings as the latest value.
                        rollout_items.push(thread_settings_applied);
                    }
                }
                self.persist_rollout_items(&rollout_items).await;
                if let Some(repair) = applied_reconstruction.repair.as_ref() {
                    self.persist_reconstruction_repair_with_policy(repair)
                        .await?;
                }
                if applied_reconstruction.should_recompute_token_usage {
                    self.recompute_token_usage(&turn_context).await;
                }

                // Forked threads should remain file-backed immediately after startup.
                self.ensure_rollout_materialized(PersistContext::Standard)
                    .await;

                // Flush after seeding history and any persisted rollout copy.
                if !is_subagent {
                    let _ = self.flush_rollout().await;
                }
                Some(turn_context)
            }
        };
        if let Some(turn_context) = turn_context
            && turn_context.config.memories.disable_on_external_context
        {
            let history = self.clone_history().await;
            if let Some(item) = history
                .raw_items()
                .find(|item| matches!(item, ResponseItem::FunctionCallOutput { call_id: None, .. }))
            {
                mark_thread_memory_mode_polluted_if_external_context(self, &turn_context, item)
                    .await;
            }
        }
        Ok(())
    }

    async fn apply_rollout_reconstruction(
        &self,
        turn_context: &TurnContext,
        rollout_items: &[RolloutItem],
    ) -> rollout_reconstruction::AppliedRolloutReconstruction {
        let reconstruction = self
            .prepare_rollout_reconstruction(turn_context, rollout_items)
            .await;
        self.install_rollout_reconstruction(turn_context, reconstruction)
            .await
    }

    #[instrument(
        level = "trace",
        skip_all,
        fields(
            thread_id = %self.thread_id(),
            rollout_item_count = rollout_items.len()
        )
    )]
    async fn prepare_rollout_reconstruction(
        &self,
        turn_context: &TurnContext,
        rollout_items: &[RolloutItem],
    ) -> rollout_reconstruction::PreparedRolloutReconstruction {
        let rollout_reconstruction::RolloutReconstruction {
            mut history,
            guardian_history,
            compacted_prefix_len,
            mut repair,
            should_recompute_token_usage,
            previous_turn_settings,
            reference_context_item,
            world_state_baseline,
            window_number,
            first_window_id,
            previous_window_id,
            window_id,
        } = self
            .reconstruct_history_from_rollout(turn_context, rollout_items)
            .await;
        let fallback_ids = {
            let state = self.state.lock().await;
            state.auto_compact_window_ids()
        };
        let effective_window_id = window_id.unwrap_or(fallback_ids.window_id);
        let effective_first_window_id = first_window_id.unwrap_or(effective_window_id);
        if let Some(repair) = repair.as_mut() {
            repair.checkpoint.window_number = Some(window_number);
            repair.checkpoint.first_window_id = Some(effective_first_window_id.to_string());
            repair.checkpoint.previous_window_id = previous_window_id.map(|id| id.to_string());
            repair.checkpoint.window_id = Some(effective_window_id.to_string());
        }
        let repair = repair.map(|repair| {
            let mut repair_items = vec![RolloutItem::Compacted(repair.checkpoint)];
            if let Some(world_state_baseline) = world_state_baseline.as_ref() {
                repair_items.push(RolloutItem::WorldState(WorldStateItem::full(
                    world_state_baseline.clone().into_object(),
                )));
            }
            if let Some(reference_context_item) = reference_context_item.as_ref() {
                repair_items.push(RolloutItem::TurnContext(reference_context_item.clone()));
            }
            rollout_reconstruction::AppliedRolloutReconstructionRepair {
                items: repair_items,
                sanitization: repair.sanitization,
                persistence: repair.persistence,
            }
        });
        // Prepare unsummarized suffix media before installing reconstructed history. Historic
        // compacted media has already been replaced with bounded references; its repair checkpoint
        // is persisted separately without rewriting the source records on this critical path.
        // Never backfill resize notices during replay; only newly recorded items may emit them,
        // so the historical model prefix remains unchanged.
        let (mut prepared_history, metadata): (Vec<_>, Vec<_>) = history
            .into_iter()
            .map(|envelope| (envelope.item, envelope.metadata))
            .unzip();
        let _ = prepare_image_response_items(
            &mut prepared_history,
            ImagePreparationMode::DetailBased,
            ImageResizeNoticeMode::Disabled,
        );
        prepare_audio_response_items(&mut prepared_history);
        assert_eq!(
            prepared_history.len(),
            metadata.len(),
            "replay media preparation must remain one-to-one when resize notices are disabled"
        );
        history = prepared_history
            .into_iter()
            .zip(metadata)
            .map(|(item, metadata)| ResponseItemEnvelope { item, metadata })
            .collect();
        rollout_reconstruction::PreparedRolloutReconstruction {
            history,
            guardian_history,
            compacted_prefix_len,
            repair,
            should_recompute_token_usage,
            previous_turn_settings,
            reference_context_item,
            world_state_baseline,
            window_number,
            first_window_id: effective_first_window_id,
            previous_window_id,
            window_id: effective_window_id,
        }
    }

    async fn install_rollout_reconstruction(
        &self,
        turn_context: &TurnContext,
        reconstruction: rollout_reconstruction::PreparedRolloutReconstruction,
    ) -> rollout_reconstruction::AppliedRolloutReconstruction {
        let rollout_reconstruction::PreparedRolloutReconstruction {
            history,
            guardian_history,
            compacted_prefix_len,
            repair,
            should_recompute_token_usage,
            previous_turn_settings,
            reference_context_item,
            world_state_baseline,
            window_number,
            first_window_id,
            previous_window_id,
            window_id,
        } = reconstruction;
        {
            let mut state = self.state.lock().await;
            state.replace_annotated_history(
                history,
                reference_context_item,
                HistoryReplacement::Reset,
            );
            state
                .history
                .restore_guardian_history(guardian_history.as_ref());
            state.history.set_compacted_prefix_len(compacted_prefix_len);
            if let Some(world_state) = world_state_baseline {
                state.history.set_world_state_baseline(world_state);
            }
            state.restore_auto_compact_window(
                window_number,
                AutoCompactWindowIds {
                    first_window_id,
                    previous_window_id,
                    window_id,
                },
            );
            state.set_previous_turn_settings(previous_turn_settings.clone());
        }
        let prefix_tokens = if matches!(
            turn_context.config.model_auto_compact_token_limit_scope,
            AutoCompactTokenLimitScope::BodyAfterPrefix
        ) {
            let history = self.clone_history().await;
            let base_instructions = self.get_base_instructions().await;
            history.estimate_token_count_with_base_instructions(&base_instructions)
        } else {
            None
        };
        if let Some(prefix_tokens) = prefix_tokens {
            self.set_auto_compact_window_estimated_prefill_for_scope(turn_context, prefix_tokens)
                .await;
        }
        rollout_reconstruction::AppliedRolloutReconstruction {
            previous_turn_settings,
            repair,
            should_recompute_token_usage,
        }
    }

    async fn set_auto_compact_window_estimated_prefill_for_scope(
        &self,
        turn_context: &TurnContext,
        tokens: i64,
    ) {
        if !matches!(
            turn_context.config.model_auto_compact_token_limit_scope,
            AutoCompactTokenLimitScope::BodyAfterPrefix
        ) {
            return;
        }

        let mut state = self.state.lock().await;
        state.set_auto_compact_window_estimated_prefill(tokens);
    }

    fn last_token_info_from_rollout(rollout_items: &[RolloutItem]) -> Option<TokenUsageInfo> {
        rollout_items.iter().rev().find_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::TokenCount(ev)) => ev.info.clone(),
            _ => None,
        })
    }

    fn last_token_usage_record_from_rollout(
        rollout_items: &[RolloutItem],
    ) -> Option<TokenUsageRecord> {
        for item in rollout_items.iter().rev() {
            match item {
                RolloutItem::TokenUsageRecord(record) => return Some(record.clone()),
                RolloutItem::Compacted(compacted) => {
                    return compacted.latest_token_usage_record.clone();
                }
                _ => {}
            }
        }
        None
    }

    async fn previous_turn_settings(&self) -> Option<PreviousTurnSettings> {
        let state = self.state.lock().await;
        state.previous_turn_settings()
    }

    #[tracing::instrument(level = "trace", skip_all)]
    pub(crate) async fn set_previous_turn_settings(
        &self,
        previous_turn_settings: Option<PreviousTurnSettings>,
    ) {
        let mut state = self.state.lock().await;
        state.set_previous_turn_settings(previous_turn_settings);
    }

    pub(crate) async fn update_settings(
        &self,
        updates: SessionSettingsUpdate,
    ) -> ConstraintResult<SessionSettingsCommit> {
        let Some(commit) = self.update_settings_if(updates, |_, _| true).await? else {
            unreachable!("unconditional settings updates must commit");
        };
        Ok(commit)
    }

    /// Evaluates the caller's synchronous predicate against the current configuration
    /// and validated candidate under the same state lock used for publication.
    /// This prevents a stale admission decision. Rejection has no settings or runtime
    /// effects.
    ///
    /// The predicate must be fast and side-effect-free. It must not block, acquire
    /// other locks, or call back into `Session`.
    async fn update_settings_if(
        &self,
        updates: SessionSettingsUpdate,
        should_commit: impl FnOnce(&SessionConfiguration, &SessionConfiguration) -> bool + Send,
    ) -> ConstraintResult<Option<SessionSettingsCommit>> {
        let notify_config_contributors = !self.services.extensions.config_contributors().is_empty();
        let (commit, previous_config, new_config, permission_profile_changed, mcp_inputs_changed) = {
            let mut state = self.state.lock().await;
            let updated = match self.apply_session_settings(&state.session_configuration, &updates)
            {
                Ok(updated) => updated,
                Err(err) => {
                    warn!("rejected session settings update: {err}");
                    return Err(err);
                }
            };

            if !should_commit(&state.session_configuration, &updated) {
                return Ok(None);
            }

            let previous_config = notify_config_contributors
                .then(|| self.build_effective_session_config(&state.session_configuration));
            let previous_permission_profile = state.session_configuration.permission_profile();
            let updated_permission_profile = updated.permission_profile();
            let permission_profile_changed =
                previous_permission_profile != updated_permission_profile;
            let mcp_inputs_changed =
                self.mcp_inputs_differ(&state.session_configuration, &updated, &updates);
            let root_service_tier_changed = updated.parent_thread_id.is_none()
                && state.session_configuration.step_settings.service_tier
                    != updated.step_settings.service_tier;
            if mcp_inputs_changed {
                self.mark_mcp_runtime_dirty();
            }
            let environment_config = updated.inferred_environment_config();
            if let Some(environments) = &updates.environments {
                self.services
                    .turn_environments
                    .update_selections(&environments.environments, &environment_config);
            } else if state.session_configuration.inferred_environment_config()
                != environment_config
            {
                self.services
                    .turn_environments
                    .update_thread_config(&environment_config);
            }
            state.session_configuration = updated;
            if root_service_tier_changed {
                self.services.agent_control.set_root_service_tier(
                    state
                        .session_configuration
                        .step_settings
                        .service_tier
                        .clone(),
                );
            }
            let new_config = notify_config_contributors
                .then(|| self.build_effective_session_config(&state.session_configuration));
            let commit = SessionSettingsCommit {
                configuration: state.session_configuration.clone(),
                snapshot: state
                    .session_configuration
                    .thread_settings_snapshot(&self.services.turn_environments.selections()),
            };
            (
                commit,
                previous_config,
                new_config,
                permission_profile_changed,
                mcp_inputs_changed,
            )
        };
        self.emit_config_changed_contributors(previous_config.as_ref(), new_config.as_ref());
        if permission_profile_changed {
            self.refresh_managed_network_proxy_for_current_permission_profile()
                .await;
        }
        if mcp_inputs_changed {
            self.schedule_mcp_prewarm();
        }
        Ok(Some(commit))
    }

    pub(crate) async fn preview_settings(
        &self,
        updates: &SessionSettingsUpdate,
    ) -> ConstraintResult<ThreadConfigSnapshot> {
        let state = self.state.lock().await;
        let configuration = self.apply_session_settings(&state.session_configuration, updates)?;
        let environments = updates.environments.as_ref().map_or_else(
            || self.services.turn_environments.selections(),
            |environments| environments.environments.clone(),
        );
        Ok(configuration.thread_config_snapshot(environments))
    }

    pub(crate) async fn thread_config_snapshot(&self) -> ThreadConfigSnapshot {
        let state = self.state.lock().await;
        state
            .session_configuration
            .thread_config_snapshot(self.services.turn_environments.selections())
    }

    pub(crate) async fn thread_settings_snapshot(&self) -> ThreadSettingsSnapshot {
        let state = self.state.lock().await;
        state
            .session_configuration
            .thread_settings_snapshot(&self.services.turn_environments.selections())
    }

    pub(crate) async fn restorable_thread_settings(&self) -> CodexThreadSettingsOverrides {
        let state = self.state.lock().await;
        state
            .session_configuration
            .restorable_thread_settings(self.services.turn_environments.selections())
    }

    pub(crate) async fn set_app_server_client_info(
        &self,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
        mcp_elicitations_auto_deny: bool,
    ) -> ConstraintResult<()> {
        self.update_settings(SessionSettingsUpdate {
            app_server_client_name,
            app_server_client_version,
            ..Default::default()
        })
        .await?;
        self.services
            .mcp_runtime
            .set_elicitations_auto_deny(mcp_elicitations_auto_deny);
        Ok(())
    }

    pub(crate) async fn instruction_sources(&self) -> Vec<PathUri> {
        self.services
            .agents_md_manager
            .get_loaded()
            .await
            .as_ref()
            .map_or_else(Vec::new, |instructions| instructions.sources().collect())
    }

    pub(crate) async fn set_session_startup_prewarm(
        &self,
        startup_prewarm: SessionStartupPrewarmHandle,
    ) {
        let mut state = self.state.lock().await;
        state.set_session_startup_prewarm(startup_prewarm);
    }

    pub(crate) async fn take_session_startup_prewarm(&self) -> Option<SessionStartupPrewarmHandle> {
        let mut state = self.state.lock().await;
        state.take_session_startup_prewarm()
    }

    pub(crate) async fn get_config(&self) -> std::sync::Arc<Config> {
        let state = self.state.lock().await;
        state
            .session_configuration
            .original_config_do_not_use
            .clone()
    }

    pub(crate) async fn user_instructions(&self) -> Option<codex_extension_api::Instructions> {
        self.services.agents_md_manager.user_instructions()
    }

    pub(crate) async fn has_mcp_server(&self, server_name: &str) -> bool {
        self.session_start_mcp_servers.contains_key(server_name)
    }

    pub(crate) async fn has_queued_mcp_server_use_context(&self, server_name: &str) -> bool {
        self.idle_pending_mcp_server_use
            .lock()
            .await
            .iter()
            .any(|queued_server_name| queued_server_name == server_name)
    }

    pub(crate) async fn queue_mcp_server_use_context_for_first_turn(&self, server_name: String) {
        self.idle_pending_mcp_server_use
            .lock()
            .await
            .push(server_name);
    }

    pub(crate) async fn render_queued_mcp_server_use_context_for_turn(
        &self,
        turn_context: &TurnContext,
    ) -> Vec<ResponseItem> {
        let server_names = std::mem::take(&mut *self.idle_pending_mcp_server_use.lock().await);
        let mut items = Vec::with_capacity(server_names.len());
        for server_name in server_names {
            if self
                .mcp_server_would_be_direct_at_session_start_for_turn_context(
                    turn_context,
                    &server_name,
                )
                .await
            {
                continue;
            }
            let text = self
                .render_mcp_server_use_context_text(server_name.as_str())
                .await;
            if self
                .latest_mcp_server_use_context_text(server_name.as_str())
                .await
                .as_deref()
                == Some(text.as_str())
            {
                continue;
            }
            items.push(ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText { text }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            });
        }
        items
    }

    pub(crate) async fn mcp_server_would_be_direct_at_session_start_for_turn_context(
        &self,
        turn_context: &TurnContext,
        server_name: &str,
    ) -> bool {
        if !self
            .session_start_mcp_servers
            .get(server_name)
            .is_some_and(|server| server.enabled() && server.config().allow_implicit_invocation)
        {
            return false;
        }
        let direct_contract_frozen = {
            self.session_start_direct_mcp_servers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some()
        };
        if direct_contract_frozen {
            return self
                .current_mcp_inventory_was_direct_at_session_start(server_name)
                .await;
        }
        let session_start_mcp_tools = self.session_start_mcp_tools.lock().await.clone();
        let exposure = crate::mcp_tool_exposure::build_mcp_tool_exposure(
            &session_start_mcp_tools,
            &session_start_mcp_tools,
            /*connectors*/ None,
            &[],
            &turn_context.config,
            &self.session_start_mcp_servers,
            crate::tools::spec_plan::search_tool_enabled(turn_context, turn_context.model_info()),
        );
        exposure
            .direct_tools
            .values()
            .any(|tool| tool.server_name == server_name)
    }

    pub(crate) async fn has_pending_input_requiring_follow_up(&self) -> bool {
        let pending_input_follow_up = {
            let active_turn_state = {
                let active = self.active_turn.lock().await;
                active
                    .as_ref()
                    .map(|active_turn| Arc::clone(&active_turn.turn_state))
            };
            let Some(active_turn_state) = active_turn_state else {
                return self.input_queue.has_pending_mailbox_items().await;
            };
            let turn_state = active_turn_state.lock().await;
            classify_pending_input_follow_up(&turn_state)
        };
        match pending_input_follow_up {
            PendingInputFollowUp::Required => true,
            PendingInputFollowUp::DeferredAtMcpBoundary => false,
            PendingInputFollowUp::NoTurnLocalFollowUp {
                accepts_mailbox_delivery,
            } => accepts_mailbox_delivery && self.input_queue.has_pending_mailbox_items().await,
        }
    }

    pub(crate) async fn active_turn_has_pending_mcp_server_use_boundary(&self) -> bool {
        let turn_state = {
            let active = self.active_turn.lock().await;
            let Some(active_turn) = active.as_ref() else {
                return false;
            };
            Arc::clone(&active_turn.turn_state)
        };
        turn_state
            .lock()
            .await
            .pending_input()
            .iter()
            .any(is_mcp_server_use_context_input_item)
    }

    pub(crate) async fn record_mcp_server_use_context_items(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        items: Vec<ResponseItem>,
    ) {
        if items.is_empty() {
            return;
        }
        if self.reference_context_item().await.is_none() {
            let cancellation_token = CancellationToken::new();
            if let Ok(step_context) = self
                .capture_step_context(Arc::clone(turn_context), &cancellation_token)
                .await
                && let Err(err) = self
                    .record_context_updates_and_set_reference_context_item(step_context.as_ref())
                    .await
            {
                warn!("failed to record context before MCP server use: {err}");
            }
        }
        self.record_conversation_items_silently(turn_context.as_ref(), &items)
            .await;
    }

    pub(crate) async fn record_active_turn_mcp_server_use_context_before_abort(
        self: &Arc<Self>,
        active_turn: &ActiveTurn,
    ) {
        let mcp_input = {
            let mut turn_state = active_turn.turn_state.lock().await;
            let pending_input = turn_state.take_pending_input();
            let mut non_mcp_input = Vec::new();
            let mut mcp_input = Vec::new();
            for item in pending_input {
                if is_mcp_server_use_context_input_item(&item) {
                    let TurnInput::ResponseItem(item) = item else {
                        continue;
                    };
                    mcp_input.push(item.into_item());
                } else {
                    non_mcp_input.push(item);
                }
            }
            turn_state.prepend_pending_input(non_mcp_input);
            mcp_input
        };
        if mcp_input.is_empty() {
            return;
        }
        let turn_context = self.new_default_turn().await;
        self.record_mcp_server_use_context_items(&turn_context, mcp_input)
            .await;
    }

    pub(crate) async fn provider(&self) -> ModelProviderInfo {
        let state = self.state.lock().await;
        state.session_configuration.provider.info().clone()
    }

    pub(crate) async fn refresh_runtime_config(&self, next_config: Config) {
        // Refresh only the user layer from the incoming snapshot. Preserve thread-local
        // layers such as request/session overrides that were present when this session
        // was created.
        let notify_config_contributors = !self.services.extensions.config_contributors().is_empty();
        let (previous_config, new_config, config) = {
            let mut state = self.state.lock().await;
            let previous_config = notify_config_contributors
                .then(|| self.build_effective_session_config(&state.session_configuration));
            let mut config = (*state.session_configuration.original_config_do_not_use).clone();
            config.active_project = next_config.active_project.clone();
            config.config_layer_stack = config
                .config_layer_stack
                .with_user_layer_from(&next_config.config_layer_stack);
            config.tool_suggest =
                resolve_tool_suggest_config_from_layer_stack(&config.config_layer_stack);
            config.mcp_servers = next_config.mcp_servers.clone();
            config.mcp_optional_startup_grace = next_config.mcp_optional_startup_grace;
            config.mcp_oauth_credentials_store_mode = next_config.mcp_oauth_credentials_store_mode;
            if let Err(err) = config.features.set_enabled(
                Feature::SecretAuthStorage,
                next_config.features.enabled(Feature::SecretAuthStorage),
            ) {
                warn!("failed to refresh MCP auth storage config: {err}");
            }
            if let Err(err) = config.features.set_enabled(
                Feature::McpOAuthRefreshCoordination,
                next_config
                    .features
                    .enabled(Feature::McpOAuthRefreshCoordination),
            ) {
                warn!("failed to refresh MCP OAuth coordination config: {err}");
            }
            let config = Arc::new(config);
            state.session_configuration.original_config_do_not_use = Arc::clone(&config);
            self.mark_mcp_runtime_dirty();
            let new_config = notify_config_contributors
                .then(|| self.build_effective_session_config(&state.session_configuration));
            (previous_config, new_config, config)
        };
        self.emit_config_changed_contributors(previous_config.as_ref(), new_config.as_ref());
        self.schedule_mcp_prewarm();
        self.refresh_hooks(config).await;
    }

    pub(crate) async fn refresh_hooks(&self, config: Arc<Config>) {
        let environments = self.services.turn_environments.snapshot().await;
        let hooks_config = build_hooks_config(
            config.as_ref(),
            self.services.plugins_manager.as_ref(),
            environments.single_local_environment(),
        )
        .await;

        let state = self.state.lock().await;
        // A newer refresh may have updated the config while this hook build was in flight.
        // Only publish hooks derived from the current config snapshot.
        if Arc::ptr_eq(
            &state.session_configuration.original_config_do_not_use,
            &config,
        ) {
            let hooks = self.hooks().reconfigured(hooks_config);
            self.services.hooks.store(Arc::new(hooks));
        }
    }

    pub(crate) async fn refresh_mcp_config(&self, next_config: Config) {
        let mut state = self.state.lock().await;
        let mut config = (*state.session_configuration.original_config_do_not_use).clone();
        config.config_layer_stack = next_config
            .config_layer_stack
            .with_user_layer_from(&config.config_layer_stack);
        config.mcp_servers = next_config.mcp_servers;
        config.mcp_optional_startup_grace = next_config.mcp_optional_startup_grace;
        config.mcp_oauth_credentials_store_mode = next_config.mcp_oauth_credentials_store_mode;
        if let Err(err) = config.features.set_enabled(
            Feature::SecretAuthStorage,
            next_config.features.enabled(Feature::SecretAuthStorage),
        ) {
            warn!("failed to refresh MCP auth storage config: {err}");
        }
        if let Err(err) = config.features.set_enabled(
            Feature::McpOAuthRefreshCoordination,
            next_config
                .features
                .enabled(Feature::McpOAuthRefreshCoordination),
        ) {
            warn!("failed to refresh MCP OAuth coordination config: {err}");
        }
        state.session_configuration.original_config_do_not_use = Arc::new(config);
        self.mark_mcp_runtime_dirty();
        drop(state);
        self.schedule_mcp_prewarm();
    }

    fn emit_config_changed_contributors(
        &self,
        previous_config: Option<&Config>,
        new_config: Option<&Config>,
    ) {
        let (Some(previous_config), Some(new_config)) = (previous_config, new_config) else {
            return;
        };
        if previous_config == new_config {
            return;
        }
        for contributor in self.services.extensions.config_contributors() {
            contributor.on_config_changed(
                &self.services.session_extension_data,
                &self.services.thread_extension_data,
                previous_config,
                new_config,
            );
        }
    }

    pub(crate) async fn reload_user_config_layer(&self) {
        // Refresh layer-backed runtime state for an existing session, including enabled plugin,
        // skill, and hook state. Derived config fields such as feature gates and legacy notify
        // settings remain session-static.
        //
        // Prefer `refresh_runtime_config()` when the host can already provide a materialized
        // config snapshot. This file-based path exists for legacy local reload flows.
        let config_toml_paths = {
            let state = self.state.lock().await;
            let config = &state.session_configuration.original_config_do_not_use;
            let user_config_paths = config
                .config_layer_stack
                .all_layers_low_to_high()
                .filter_map(|layer| match &layer.name {
                    ConfigLayerSource::User { file, .. } => Some(file.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if user_config_paths.is_empty() {
                vec![
                    state
                        .session_configuration
                        .codex_home
                        .join(CONFIG_TOML_FILE),
                ]
            } else {
                user_config_paths
            }
        };

        let mut reloaded_user_configs = Vec::with_capacity(config_toml_paths.len());
        for config_toml_path in config_toml_paths {
            let user_config = match std::fs::read_to_string(&config_toml_path) {
                Ok(contents) => match toml::from_str::<toml::Value>(&contents) {
                    Ok(config) => config,
                    Err(err) => {
                        warn!("failed to parse user config while reloading layer: {err}");
                        return;
                    }
                },
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    toml::Value::Table(Default::default())
                }
                Err(err) => {
                    warn!("failed to read user config while reloading layer: {err}");
                    return;
                }
            };
            reloaded_user_configs.push((config_toml_path, user_config));
        }

        let next_config = {
            let state = self.state.lock().await;
            let mut config = (*state.session_configuration.original_config_do_not_use).clone();
            for (config_toml_path, user_config) in reloaded_user_configs {
                let config_layer_stack = match config
                    .config_layer_stack
                    .with_user_config(&config_toml_path, user_config)
                {
                    Ok(config_layer_stack) => config_layer_stack,
                    Err(err) => {
                        warn!("failed to validate user config while reloading layer: {err}");
                        return;
                    }
                };
                config.config_layer_stack = config_layer_stack;
            }
            config.tool_suggest =
                resolve_tool_suggest_config_from_layer_stack(&config.config_layer_stack);
            config
        };
        self.services.skills_service.clear_cache();
        self.services.plugins_manager.clear_cache();
        self.refresh_runtime_config(next_config).await;
    }

    /// Record a terminal CodexErr before the app-server completion notification is reduced.
    pub(crate) fn track_turn_codex_error(&self, turn_context: &TurnContext, error: &CodexErr) {
        self.services
            .analytics_events_client
            .track_turn_codex_error(TurnCodexErrorFact::from_codex_err(
                self.thread_id.to_string(),
                turn_context.sub_id.clone(),
                error,
            ));
    }

    /// Persist the event to rollout and send it to clients.
    pub(crate) async fn send_event(&self, turn_context: &TurnContext, msg: EventMsg) {
        // Keep the subagent completion delivery state machine out of every caller's future.
        Box::pin(self.send_event_with_primary_delivery(
            turn_context,
            msg,
            EventPersistence::Queued,
            || {},
        ))
        .await;
    }

    async fn send_event_with_primary_delivery<F>(
        &self,
        turn_context: &TurnContext,
        msg: EventMsg,
        persistence: EventPersistence,
        on_primary_delivery: F,
    ) where
        F: FnOnce() + Send,
    {
        self.send_event_with_primary_delivery_and_rollout_suffix(
            turn_context,
            msg,
            persistence,
            Vec::new(),
            on_primary_delivery,
        )
        .await;
    }

    async fn send_event_with_primary_delivery_and_rollout_suffix<F>(
        &self,
        turn_context: &TurnContext,
        msg: EventMsg,
        persistence: EventPersistence,
        rollout_suffix: Vec<RolloutItem>,
        on_primary_delivery: F,
    ) where
        F: FnOnce() + Send,
    {
        let legacy_source = msg.clone();
        let terminal_presentation = self
            .prepare_sub_agent_terminal_presentation(turn_context, &legacy_source)
            .await;
        if let EventMsg::Error(error) = &legacy_source
            && error.affects_turn_status()
        {
            turn_context
                .terminal_error
                .lock()
                .await
                .replace(error.clone());
        }
        self.services
            .rollout_thread_trace
            .record_codex_turn_event(&turn_context.sub_id, &legacy_source);
        self.services
            .rollout_thread_trace
            .record_tool_call_event(turn_context.sub_id.clone(), &legacy_source);
        let event = Event {
            id: turn_context.sub_id.clone(),
            msg,
        };
        // Private reviewers have no app-server listener; publicly resumed Guardian threads do.
        if matches!(
            &turn_context.session_source,
            SessionSource::SubAgent(SubAgentSource::Other(name))
                if name == crate::guardian::GUARDIAN_REVIEWER_NAME
        ) && self.services.analytics_events_client.is_enabled()
            && turn_context.parent_thread_id.is_some()
            && self
                .state
                .lock()
                .await
                .session_configuration
                .trusted_guardian_reviewer
        {
            self.services
                .analytics_events_client
                .track_guardian_session_event(self.thread_id, &event);
        }
        let has_rollout_suffix = !rollout_suffix.is_empty();
        let primary_delivery = self
            .send_event_raw_prepared_with_rollout_suffix(event, persistence, rollout_suffix)
            .await;
        let primary_committed =
            if primary_delivery.persisted && (primary_delivery.delivered || has_rollout_suffix) {
                true
            } else if let EventMsg::ItemCompleted(event) = &legacy_source
                && event.item.is_sub_agent_completion_presentation()
            {
                // This lookup intentionally covers completed `wait_agent` items as well as
                // standalone completion messages: both own subagent presentation through
                // `is_sub_agent_completion_presentation`. If a flush reports an error after the
                // canonical batch committed, its stable item and turn IDs let the wait retain
                // final-outcome presentation ownership instead of releasing it to duplicate
                // automatic delivery.
                let item_id = event.item.id();
                match self
                    .persisted_sub_agent_completion_presentation(&item_id, &event.turn_id)
                    .await
                {
                    Ok(presentation) => presentation
                        .item_completed
                        .is_some_and(|persisted| persisted.turn_id == event.turn_id),
                    Err(err) => {
                        warn!(
                            item_id = %item_id,
                            turn_id = %event.turn_id,
                            "failed to reconcile subagent completion presentation: {err}"
                        );
                        false
                    }
                }
            } else {
                false
            };
        if primary_committed {
            on_primary_delivery();
        }
        self.maybe_notify_parent_of_terminal_turn(
            turn_context,
            &legacy_source,
            terminal_presentation,
        )
        .await;
        self.maybe_mirror_event_text_to_realtime(&legacy_source)
            .await;
        self.maybe_clear_realtime_handoff_for_event(&legacy_source)
            .await;

        let show_raw_agent_reasoning = self.show_raw_agent_reasoning();
        for legacy in legacy_source.as_legacy_events(show_raw_agent_reasoning) {
            self.services
                .rollout_thread_trace
                .record_tool_call_event(turn_context.sub_id.clone(), &legacy);
            let legacy_event = Event {
                id: turn_context.sub_id.clone(),
                msg: legacy,
            };
            self.send_event_raw(legacy_event).await;
        }
    }

    /// Forwards terminal turn events from spawned MultiAgentV2 children to their direct parent.
    async fn maybe_notify_parent_of_terminal_turn(
        &self,
        turn_context: &TurnContext,
        msg: &EventMsg,
        terminal_presentation: Option<AgentTerminalPresentation>,
    ) {
        if turn_context.multi_agent_version != MultiAgentVersion::V2 {
            return;
        }

        if !matches!(msg, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)) {
            return;
        }

        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            agent_path: Some(child_agent_path),
            ..
        }) = &turn_context.session_source
        else {
            return;
        };

        let status = match turn_context.terminal_error.lock().await.take() {
            Some(error) => AgentStatus::Errored(error.message),
            None => {
                let Some(status) = agent_status_from_event(msg) else {
                    return;
                };
                status
            }
        };
        if !is_final(&status) {
            return;
        }
        let Some(terminal_presentation) = terminal_presentation else {
            return;
        };

        self.forward_child_completion_to_parent(
            turn_context,
            *parent_thread_id,
            child_agent_path,
            status,
            terminal_presentation,
        )
        .await;
    }

    /// Sends the standard completion envelope from a spawned MultiAgentV2 child to its parent.
    async fn forward_child_completion_to_parent(
        &self,
        turn_context: &TurnContext,
        parent_thread_id: ThreadId,
        child_agent_path: &codex_protocol::AgentPath,
        status: AgentStatus,
        terminal_presentation: AgentTerminalPresentation,
    ) {
        let accepted_completion_delivery =
            terminal_presentation.take_accepted_completion_delivery();
        let admission = if accepted_completion_delivery.is_some() {
            CompletionSubmissionAdmission::Accepted
        } else {
            CompletionSubmissionAdmission::Ordinary
        };
        let Some(parent_agent_path) = child_agent_path
            .as_str()
            .rsplit_once('/')
            .and_then(|(parent, _)| codex_protocol::AgentPath::try_from(parent).ok())
        else {
            return;
        };

        if matches!(status, AgentStatus::Completed(_))
            && let Some(parent_turn_id) = turn_context.turn_metadata_state.parent_turn_id()
        {
            let initiating_thread_id = match turn_context
                .turn_metadata_state
                .initiating_agent_path()
            {
                Some(initiating_agent_path) if initiating_agent_path != &parent_agent_path => self
                    .services
                    .agent_control
                    .resolve_agent_reference(
                        self.thread_id,
                        &turn_context.session_source,
                        initiating_agent_path.as_str(),
                    )
                    .await
                    .inspect_err(|err| {
                        debug!(
                            "failed to resolve completed activity initiator {initiating_agent_path}: {err}"
                        );
                    })
                    .ok(),
                _ => Some(parent_thread_id),
            };
            if let Some(initiating_thread_id) = initiating_thread_id
                && let Err(err) = self
                    .services
                    .agent_control
                    .emit_sub_agent_activity(
                        initiating_thread_id,
                        parent_turn_id,
                        SubAgentActivityItem {
                            id: format!("subagent-completed-{}", turn_context.sub_id),
                            kind: SubAgentActivityKind::Completed,
                            agent_thread_id: self.thread_id,
                            agent_path: child_agent_path.clone(),
                        },
                    )
                    .await
            {
                debug!(
                    "failed to emit completed activity to initiating thread {initiating_thread_id}: {err}"
                );
            }
        }

        let Some(message) = format_inter_agent_completion_message(
            parent_agent_path.clone(),
            child_agent_path.clone(),
            &status,
        ) else {
            return;
        };
        // `communication` owns the message. Keep a second copy only when the
        // recorder will actually need it after parent delivery succeeds.
        let trace_message = self
            .services
            .rollout_thread_trace
            .is_enabled()
            .then(|| message.clone());
        let mut communication = InterAgentCommunication::new(
            child_agent_path.clone(),
            parent_agent_path,
            Vec::new(),
            message,
            /*trigger_turn*/ false,
        );
        communication.id = Some(terminal_presentation.completion_context_response_item_id());
        let context =
            AgentCommunicationContext::new(AgentCommunicationKind::Result, self.thread_id);
        let completion_communication = communication.clone();
        let parent_thread = match self
            .services
            .agent_control
            .send_inter_agent_completion_communication(
                parent_thread_id,
                communication,
                context,
                &terminal_presentation,
                admission,
            )
            .await
        {
            Ok((_submission_id, parent_thread)) => parent_thread,
            Err(err) => {
                debug!("failed to notify parent thread {parent_thread_id}: {err}");
                return;
            }
        };
        if !parent_thread
            .persist_inter_agent_completion_context_without_turn(completion_communication)
            .await
        {
            return;
        }
        let wait_owns_presentation = terminal_presentation.wait_owns_presentation().await;
        if !wait_owns_presentation {
            match accepted_completion_delivery {
                Some(completion_delivery) => {
                    parent_thread
                        .emit_accepted_sub_agent_completion_without_turn(
                            child_agent_path.as_str(),
                            &status,
                            SubAgentCompletionModelVisibility::Visible,
                            completion_delivery,
                        )
                        .await;
                }
                None => {
                    parent_thread
                        .emit_sub_agent_completion_without_turn(
                            child_agent_path.as_str(),
                            &status,
                            SubAgentCompletionModelVisibility::Visible,
                        )
                        .await;
                }
            }
        }
        if let Some(message) = trace_message {
            self.services
                .rollout_thread_trace
                .record_agent_result_interaction(
                    turn_context.sub_id.as_str(),
                    parent_thread_id,
                    &AgentResultTracePayload {
                        child_agent_path: child_agent_path.as_str(),
                        message: &message,
                        status: &status,
                    },
                );
        }
    }

    async fn maybe_mirror_event_text_to_realtime(&self, msg: &EventMsg) {
        if self.conversation.running_state().await.is_none() {
            return;
        }
        match msg {
            EventMsg::ItemStarted(event) => {
                if let TurnItem::AgentMessage(item) = &event.item
                    && !item.is_attributed_agent_input_presentation()
                {
                    self.conversation
                        .register_handoff_stream_item(
                            item.id.clone(),
                            item.phase.clone(),
                            agent_message_text(item),
                        )
                        .await;
                }
                return;
            }
            EventMsg::AgentMessageContentDelta(event) => {
                if let Err(err) = self
                    .conversation
                    .stream_handoff_delta(&event.item_id, event.delta.clone())
                    .await
                {
                    debug!("failed to stream event text to realtime conversation: {err}");
                }
                return;
            }
            EventMsg::ItemCompleted(event) => {
                if let TurnItem::AgentMessage(item) = &event.item
                    && (item.is_attributed_agent_input_presentation()
                        || self.conversation.finish_handoff_stream_item(&item.id).await)
                {
                    return;
                }
            }
            _ => {}
        }
        let Some((text, phase)) = realtime_text_for_event(msg) else {
            return;
        };
        if let Err(err) = self.conversation.handoff_out(text, phase).await {
            debug!("failed to mirror event text to realtime conversation: {err}");
        }
    }

    async fn maybe_clear_realtime_handoff_for_event(&self, msg: &EventMsg) {
        if !matches!(msg, EventMsg::TurnComplete(_)) {
            return;
        }
        if let Err(err) = self.conversation.handoff_complete().await {
            debug!("failed to finalize realtime handoff output: {err}");
        }
        self.conversation.clear_active_handoff().await;
    }

    pub(crate) async fn send_event_raw(&self, event: Event) {
        self.send_event_raw_with_persistence(event, /*persist*/ true)
            .await;
    }

    /// Persist one standalone event through a durability barrier before publishing it.
    pub(crate) async fn send_event_raw_flushed(&self, event: Event) -> std::io::Result<()> {
        if self.live_thread().is_none() {
            return Err(std::io::Error::other(
                "session persistence is disabled for standalone event",
            ));
        }
        self.prepare_raw_sub_agent_terminal_presentation(&event);
        let delivery = self
            .send_event_raw_prepared_with_rollout_suffix(
                event,
                EventPersistence::Flushed,
                Vec::new(),
            )
            .await;
        if delivery.persisted {
            Ok(())
        } else {
            Err(std::io::Error::other(
                "standalone event rollout append failed",
            ))
        }
    }

    /// Delivers an event without creating a local rollout for a thread that has not materialized.
    pub(crate) async fn send_event_raw_without_materializing_rollout(&self, event: Event) {
        let persist = match self.current_rollout_path().await {
            Ok(Some(path)) => codex_rollout::existing_rollout_path(&path).await.is_some(),
            Ok(None) => true,
            Err(err) => {
                warn!("failed to check whether thread persistence is materialized: {err}");
                true
            }
        };
        self.send_event_raw_with_persistence(event, persist).await;
    }

    async fn send_event_raw_with_persistence(&self, event: Event, persist: bool) {
        self.prepare_raw_sub_agent_terminal_presentation(&event);
        let persistence = if persist {
            EventPersistence::Queued
        } else {
            EventPersistence::Skip
        };
        self.send_event_raw_prepared(event, persistence).await;
    }

    async fn send_event_raw_prepared(&self, event: Event, persistence: EventPersistence) -> bool {
        self.send_event_raw_prepared_with_rollout_suffix(event, persistence, Vec::new())
            .await
            .committed()
    }

    async fn send_event_raw_prepared_with_rollout_suffix(
        &self,
        event: Event,
        persistence: EventPersistence,
        rollout_suffix: Vec<RolloutItem>,
    ) -> RawEventDelivery {
        debug_assert!(
            rollout_suffix.is_empty() || matches!(persistence, EventPersistence::Flushed)
        );
        // Keep realtime reduction, canonical append, and delivery in the same order.
        // This lock must not acquire SessionState or ActiveTurn: event producers can
        // already hold those locks. Host presentation policies are synchronous.
        let mut realtime_history = match &self.realtime_history {
            Some(history) => {
                let history = history.lock().await;
                history.should_observe(&event.msg).then_some(history)
            }
            None => None,
        };
        self.services.mcp_runtime.observe_event(&event.msg);
        let (before_event, after_event) = match realtime_history.as_mut() {
            Some(history) => {
                let effects = history.observe(&event.msg);
                match effects.order {
                    RealtimeEventOrder::BeforeEvent => (Some(effects), None),
                    RealtimeEventOrder::AfterEvent => (None, Some(effects)),
                }
            }
            None => (None, None),
        };
        if let Some(effects) = before_event
            && let Err(error) = self.send_realtime_history_effects(&event.id, effects).await
        {
            warn!("failed to persist realtime history: {error}");
        }
        // Persist the event into rollout storage; the store applies its persistence policy.
        let persistence_result = match persistence {
            EventPersistence::Skip => Ok(()),
            EventPersistence::Queued => {
                debug_assert!(rollout_suffix.is_empty());
                self.try_persist_rollout_items(&[RolloutItem::EventMsg(event.msg.clone())])
                    .await
            }
            EventPersistence::Flushed => {
                let mut rollout_items = vec![RolloutItem::EventMsg(event.msg.clone())];
                rollout_items.extend(rollout_suffix);
                match self.live_thread() {
                    Some(live_thread) => {
                        live_thread
                            .append_items_and_flush_canonical(&rollout_items)
                            .await
                    }
                    None => Ok(()),
                }
            }
        };
        let persisted = match persistence_result {
            Ok(()) => true,
            Err(err) => {
                error!("failed to record rollout items: {err:#}");
                false
            }
        };
        // Commentary is observable only after its source item is durable. A final turn outcome is
        // different: the live child status becomes final even when its rollout append fails, so
        // suppressing the response event would strand separately owned V1 observers forever.
        // Let those observers attempt their own durable parent-side delivery; canonical recovery
        // from the child remains unavailable until the child rollout can be repaired.
        let live_terminal = agent_status_from_event(&event.msg).is_some_and(|status| {
            is_final(&status)
                && matches!(
                    &event.msg,
                    EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
                )
        });
        let response_event_publication = if persisted || live_terminal {
            AgentResponseEventPublication::Publish
        } else {
            AgentResponseEventPublication::Skip
        };
        self.services
            .rollout_thread_trace
            .record_protocol_event(&event.msg);
        if let Some(effects) = after_event
            && let Err(error) = self.send_realtime_history_effects(&event.id, effects).await
        {
            warn!("failed to persist realtime history: {error}");
        }
        let delivered = self
            .deliver_event_raw_prepared(event, response_event_publication)
            .await;
        RawEventDelivery {
            persisted,
            delivered,
        }
    }

    async fn deliver_event_raw(&self, event: Event) {
        self.prepare_raw_sub_agent_terminal_presentation(&event);
        let _ = self
            .deliver_event_raw_prepared(event, AgentResponseEventPublication::Skip)
            .await;
    }

    async fn deliver_event_raw_prepared(
        &self,
        event: Event,
        response_event_publication: AgentResponseEventPublication,
    ) -> bool {
        let publish_event = match response_event_publication {
            AgentResponseEventPublication::Skip => self.publish_agent_envelope_status(&event),
            AgentResponseEventPublication::Publish => {
                self.publish_agent_response_event_and_status(&event)
            }
        };
        if !publish_event {
            return false;
        }
        if let Err(e) = self.tx_event.send(event).await {
            debug!("dropping event because channel is closed: {e}");
            return false;
        }
        true
    }

    pub(crate) async fn emit_turn_item_started(&self, turn_context: &TurnContext, item: &TurnItem) {
        let started_at_ms = turn_context
            .turn_timing_state
            .record_item_started(item.id(), now_unix_timestamp_ms())
            .await;
        self.send_event(
            turn_context,
            EventMsg::ItemStarted(ItemStartedEvent {
                thread_id: self.thread_id,
                turn_id: turn_context.sub_id.clone(),
                item: item.clone(),
                started_at_ms,
            }),
        )
        .await;
    }

    /// Emits a live-only compaction progress notification.
    ///
    /// Use this for transient UI progress labels that should not become part of
    /// persisted replay.
    pub(crate) async fn emit_transient_context_compaction_status(
        &self,
        turn_context: &TurnContext,
        item_id: String,
        message: String,
    ) {
        self.deliver_event_raw(Event {
            id: turn_context.sub_id.clone(),
            msg: EventMsg::ContextCompactionStatus(ContextCompactionStatusEvent {
                item_id,
                message,
            }),
        })
        .await;
    }

    pub(crate) async fn emit_turn_item_completed(
        &self,
        turn_context: &TurnContext,
        item: TurnItem,
    ) {
        record_turn_ttfm_metric(turn_context, &item).await;
        let completed_at_ms = now_unix_timestamp_ms();
        let item_id = item.id();
        let started_at_ms = turn_context
            .turn_timing_state
            .take_item_started(&item_id)
            .await
            .unwrap_or_else(|| {
                warn!(
                    thread_id = %self.thread_id,
                    turn_id = %turn_context.sub_id,
                    item_id = %item_id,
                    "item completed without a recorded start timestamp"
                );
                completed_at_ms
            });
        self.send_event(
            turn_context,
            EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: self.thread_id,
                turn_id: turn_context.sub_id.clone(),
                item,
                started_at_ms: Some(started_at_ms),
                completed_at_ms,
            }),
        )
        .await;
    }

    pub(crate) async fn emit_turn_item_completed_with_primary_delivery<F>(
        &self,
        turn_context: &TurnContext,
        item: TurnItem,
        on_primary_delivery: F,
    ) where
        F: FnOnce() + Send,
    {
        record_turn_ttfm_metric(turn_context, &item).await;
        self.send_event_with_primary_delivery(
            turn_context,
            EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: self.thread_id,
                turn_id: turn_context.sub_id.clone(),
                item,
                started_at_ms: None,
                completed_at_ms: now_unix_timestamp_ms(),
            }),
            EventPersistence::Flushed,
            on_primary_delivery,
        )
        .await;
    }

    pub(crate) async fn emit_turn_item_completed_with_primary_delivery_and_rollout_suffix<F>(
        &self,
        turn_context: &TurnContext,
        item: TurnItem,
        rollout_suffix: Vec<RolloutItem>,
        on_primary_delivery: F,
    ) where
        F: FnOnce() + Send,
    {
        record_turn_ttfm_metric(turn_context, &item).await;
        self.send_event_with_primary_delivery_and_rollout_suffix(
            turn_context,
            EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: self.thread_id,
                turn_id: turn_context.sub_id.clone(),
                item,
                started_at_ms: None,
                completed_at_ms: now_unix_timestamp_ms(),
            }),
            EventPersistence::Flushed,
            rollout_suffix,
            on_primary_delivery,
        )
        .await;
    }

    /// Adds an execpolicy amendment to both the in-memory and on-disk policies so future
    /// commands can use the newly approved prefix.
    pub(crate) async fn persist_execpolicy_amendment(
        &self,
        amendment: &ExecPolicyAmendment,
    ) -> Result<(), ExecPolicyUpdateError> {
        let codex_home = self
            .state
            .lock()
            .await
            .session_configuration
            .codex_home()
            .clone();

        self.services
            .exec_policy
            .append_amendment_and_update(&codex_home, amendment)
            .await?;

        Ok(())
    }

    pub(crate) async fn turn_context_for_sub_id(&self, sub_id: &str) -> Option<Arc<TurnContext>> {
        let active = self.active_turn.lock().await;
        active
            .as_ref()
            .and_then(|turn| turn.task.as_ref())
            .filter(|task| task.turn_context.sub_id == sub_id)
            .map(|task| Arc::clone(&task.turn_context))
    }

    async fn active_turn_context_and_cancellation_token(
        &self,
    ) -> Option<(Arc<TurnContext>, CancellationToken)> {
        let active = self.active_turn.lock().await;
        let task = active.as_ref()?.task.as_ref()?;
        Some((
            Arc::clone(&task.turn_context),
            task.cancellation_token.child_token(),
        ))
    }

    pub(crate) async fn persist_network_policy_amendment(
        &self,
        amendment: &NetworkPolicyAmendment,
        network_approval_context: &NetworkApprovalContext,
        on_policy_applied: impl FnOnce() + Send,
    ) -> anyhow::Result<()> {
        let _refresh_guard = self
            .managed_network_proxy_refresh_lock
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("managed network proxy refresh semaphore closed"))?;
        let host =
            Self::validated_network_policy_amendment_host(amendment, network_approval_context)?;
        let codex_home = self
            .state
            .lock()
            .await
            .session_configuration
            .codex_home()
            .clone();
        let execpolicy_amendment =
            execpolicy_network_rule_amendment(amendment, network_approval_context, &host);
        let mut on_policy_applied = Some(on_policy_applied);

        if let Some(started_network_proxy) = self.services.network_proxy.load_full() {
            let proxy = started_network_proxy.proxy();
            match amendment.action {
                NetworkPolicyRuleAction::Allow => proxy
                    .add_allowed_domain(&host)
                    .await
                    .map_err(|err| anyhow::anyhow!("failed to update runtime allowlist: {err}"))?,
                NetworkPolicyRuleAction::Deny => proxy
                    .add_denied_domain(&host)
                    .await
                    .map_err(|err| anyhow::anyhow!("failed to update runtime denylist: {err}"))?,
            }
            // Active enforcement changed successfully. Notify the owner before
            // the next fallible await so cancellation cannot contradict it.
            if let Some(on_policy_applied) = on_policy_applied.take() {
                on_policy_applied();
            }
        }

        self.services
            .exec_policy
            .append_network_rule_and_update(
                &codex_home,
                &host,
                execpolicy_amendment.protocol,
                execpolicy_amendment.decision,
                Some(execpolicy_amendment.justification),
            )
            .await
            .map_err(|err| {
                anyhow::anyhow!("failed to persist network policy amendment to execpolicy: {err}")
            })?;

        // Without a running proxy, persistence is the first effective policy change.
        if let Some(on_policy_applied) = on_policy_applied {
            on_policy_applied();
        }

        Ok(())
    }

    fn validated_network_policy_amendment_host(
        amendment: &NetworkPolicyAmendment,
        network_approval_context: &NetworkApprovalContext,
    ) -> anyhow::Result<String> {
        let approved_host = normalize_host(&network_approval_context.host);
        let amendment_host = normalize_host(&amendment.host);
        if amendment_host != approved_host {
            return Err(anyhow::anyhow!(
                "network policy amendment host '{}' does not match approved host '{}'",
                amendment.host,
                network_approval_context.host
            ));
        }
        Ok(approved_host)
    }

    pub(crate) async fn record_network_policy_amendment_message(
        &self,
        sub_id: &str,
        amendment: &NetworkPolicyAmendment,
    ) {
        let message: ResponseItem = ContextualUserFragment::into(NetworkRuleSaved::new(amendment));
        let turn_context = self.turn_context_for_sub_id(sub_id).await;
        self.inject_no_new_turn(vec![message], turn_context.as_deref())
            .await;
    }

    /// Emit an exec approval request event and await the user's decision.
    ///
    /// The request is keyed by `call_id` + `approval_id` so matching responses
    /// are delivered to the correct in-flight turn. If the pending approval is
    /// cleared before a response arrives, treat it as an abort so interrupted
    /// turns do not continue on a synthetic denial.
    ///
    /// Note that if `available_decisions` is `None`, then the other fields will
    /// be used to derive the available decisions via
    /// [ExecApprovalRequestEvent::default_available_decisions].
    #[allow(clippy::too_many_arguments)]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn request_command_approval(
        &self,
        turn_context: &TurnContext,
        kind: ExecApprovalKind,
        call_id: String,
        approval_id: Option<String>,
        environment_id: Option<String>,
        command: Vec<String>,
        cwd: PathUri,
        reason: Option<String>,
        network_approval_context: Option<NetworkApprovalContext>,
        proposed_execpolicy_amendment: Option<ExecPolicyAmendment>,
        additional_permissions: Option<AdditionalPermissionProfile>,
        available_decisions: Option<Vec<ReviewDecision>>,
        plugin_attribution_override: Option<PluginCommandAttribution>,
    ) -> ReviewDecision {
        //  command-level approvals use `call_id`.
        // `approval_id` identifies subcommand callbacks and stdin writes.
        let effective_approval_id = approval_id.clone().unwrap_or_else(|| call_id.clone());
        let monotonic_now = tokio::time::Instant::now();
        let wall_now_ms = now_unix_timestamp_ms();
        if turn_context.config.approval_timeout_ms == Some(0) {
            return ReviewDecision::TimedOut;
        }
        let _elicitation = self.services.elicitations.register();
        let started_at_ms = wall_now_ms;
        let expires_at_ms = turn_context.config.approval_timeout_ms.map(|timeout_ms| {
            wall_now_ms.saturating_add(i64::try_from(timeout_ms).unwrap_or(i64::MAX))
        });
        let approval_deadline = turn_context
            .config
            .approval_timeout_ms
            .map(|timeout_ms| saturating_instant_add_ms(monotonic_now, timeout_ms));
        // Add the tx_approve callback to the map before sending the request.
        let (tx_approve, mut rx_approve) = oneshot::channel();
        let approval_identity = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.insert_pending_approval(
                        effective_approval_id.clone(),
                        approval_deadline,
                        tx_approve,
                    )
                }
                None => None,
            }
        };
        let Some(approval_identity) = approval_identity else {
            warn!("Pending approval already exists for call_id: {effective_approval_id}");
            return ReviewDecision::Abort;
        };

        let parsed_cmd = parse_command(&command);
        let proposed_network_policy_amendments = network_approval_context.as_ref().map(|context| {
            vec![
                NetworkPolicyAmendment {
                    host: context.host.clone(),
                    action: NetworkPolicyRuleAction::Allow,
                },
                NetworkPolicyAmendment {
                    host: context.host.clone(),
                    action: NetworkPolicyRuleAction::Deny,
                },
            ]
        });
        let available_decisions = available_decisions.unwrap_or_else(|| {
            ExecApprovalRequestEvent::default_available_decisions(
                network_approval_context.as_ref(),
                proposed_execpolicy_amendment.as_ref(),
                proposed_network_policy_amendments.as_deref(),
                additional_permissions.as_ref(),
            )
        });
        let plugin_attribution = plugin_attribution_override.or_else(|| {
            cwd.to_abs_path()
                .ok()
                .and_then(|cwd| turn_context.plugin_attribution_for_command(&command, &cwd))
        });
        let (plugin_id, script_path) = plugin_attribution
            .as_ref()
            .map(PluginCommandAttribution::serialized_fields)
            .unzip();
        let event = EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
            kind,
            call_id,
            plugin_id,
            script_path,
            approval_id,
            turn_id: turn_context.sub_id.clone(),
            environment_id,
            started_at_ms,
            expires_at_ms,
            command,
            cwd: cwd.into(),
            reason,
            network_approval_context,
            proposed_execpolicy_amendment,
            proposed_network_policy_amendments,
            additional_permissions,
            available_decisions: Some(available_decisions),
            parsed_cmd,
        });
        self.send_event(turn_context, event).await;
        let Some(approval_deadline) = approval_deadline else {
            return rx_approve.await.unwrap_or(ReviewDecision::Abort);
        };
        tokio::select! {
            biased;
            () = tokio::time::sleep_until(approval_deadline) => {
                let entry = {
                    let mut active = self.active_turn.lock().await;
                    match active.as_mut() {
                        Some(at) => {
                            let mut ts = at.turn_state.lock().await;
                            ts.remove_pending_approval_if_same(
                                &effective_approval_id,
                                &approval_identity,
                            )
                        }
                        None => None,
                    }
                };
                if entry.is_some() {
                    ReviewDecision::TimedOut
                } else {
                    rx_approve.await.unwrap_or(ReviewDecision::Abort)
                }
            }
            decision = &mut rx_approve => match decision {
                Ok(decision) => decision,
                Err(_) if tokio::time::Instant::now() >= approval_deadline => {
                    ReviewDecision::TimedOut
                }
                Err(_) => ReviewDecision::Abort,
            },
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn request_patch_approval(
        &self,
        turn_context: &TurnContext,
        call_id: String,
        changes: HashMap<PathBuf, FileChange>,
        reason: Option<String>,
        grant_root: Option<PathBuf>,
    ) -> ReviewDecision {
        let _elicitation = self.services.elicitations.register();
        // Add the tx_approve callback to the map before sending the request.
        let (tx_approve, rx_approve) = oneshot::channel();
        let approval_id = call_id.clone();
        let approval_identity = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.insert_pending_approval(
                        approval_id.clone(),
                        /*deadline*/ None,
                        tx_approve,
                    )
                }
                None => None,
            }
        };
        if approval_identity.is_none() {
            warn!("Pending approval already exists for call_id: {approval_id}");
            return ReviewDecision::Abort;
        }

        let event = EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
            call_id,
            turn_id: turn_context.sub_id.clone(),
            started_at_ms: now_unix_timestamp_ms(),
            changes,
            reason,
            grant_root,
        });
        self.send_event(turn_context, event).await;
        rx_approve.await.unwrap_or(ReviewDecision::Abort)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn request_permissions_for_environment(
        self: &Arc<Self>,
        step_context: &Arc<StepContext>,
        call_id: String,
        args: RequestPermissionsArgs,
        environment_selection: TurnEnvironmentSelection,
        cancellation_token: CancellationToken,
    ) -> Option<RequestPermissionsResponse> {
        let turn_context = &step_context.turn;
        let approval_policy = step_context.settings.approval_policy();
        let approvals_reviewer = step_context.settings.approvals_reviewer();
        let Some(environment) = step_context
            .environments
            .turn_environments()
            .find(|candidate| {
                candidate.selection.environment_id == environment_selection.environment_id
            })
            .cloned()
        else {
            return Some(RequestPermissionsResponse {
                permissions: RequestPermissionProfile::default(),
                scope: PermissionGrantScope::Turn,
                strict_auto_review: false,
            });
        };
        let cwd = environment.cwd();
        match approval_policy {
            AskForApproval::Never => {
                return Some(RequestPermissionsResponse {
                    permissions: RequestPermissionProfile::default(),
                    scope: PermissionGrantScope::Turn,
                    strict_auto_review: false,
                });
            }
            AskForApproval::Granular(granular_config)
                if !granular_config.allows_request_permissions() =>
            {
                return Some(RequestPermissionsResponse {
                    permissions: RequestPermissionProfile::default(),
                    scope: PermissionGrantScope::Turn,
                    strict_auto_review: false,
                });
            }
            AskForApproval::OnRequest
            | AskForApproval::UnlessTrusted
            | AskForApproval::Granular(_) => {}
        }

        let requested_permissions = args.permissions;
        let sandbox_context = environment.sandbox_context(/*additional_permissions*/ None);
        let Some(context) = sandbox_context.policy_context() else {
            return Some(RequestPermissionsResponse {
                permissions: RequestPermissionProfile::default(),
                scope: PermissionGrantScope::Turn,
                strict_auto_review: false,
            });
        };
        if crate::guardian::routes_approval_policy_to_guardian(approval_policy, approvals_reviewer)
        {
            let originating_turn_state = {
                let active = self.active_turn.lock().await;
                active.as_ref().map(|active| Arc::clone(&active.turn_state))
            };
            let action = ApprovalAction::RequestPermissions {
                id: call_id.clone(),
                turn_id: turn_context.sub_id.clone(),
                reason: args.reason,
                permissions: requested_permissions.clone(),
            };
            let approval_context = ApprovalContext {
                review_context: crate::guardian::GuardianReviewContext::from(step_context),
                cancellation_token: Some(cancellation_token.clone()),
                call_id,
                tool_name: ToolName::plain("request_permissions"),
                strict_auto_review: false,
                approval_reason: None,
                retry_reason: None,
                network_approval_context: None,
            };
            let decision = tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => return None,
                decision = self.request_guardian_approval(
                    action,
                    &approval_context,
                ) => decision,
            };
            let response = match decision {
                ReviewDecision::Approved | ReviewDecision::ApprovedExecpolicyAmendment { .. } => {
                    RequestPermissionsResponse {
                        permissions: requested_permissions.clone(),
                        scope: PermissionGrantScope::Turn,
                        strict_auto_review: false,
                    }
                }
                ReviewDecision::ApprovedForSession => RequestPermissionsResponse {
                    permissions: requested_permissions.clone(),
                    scope: PermissionGrantScope::Session,
                    strict_auto_review: false,
                },
                ReviewDecision::NetworkPolicyAmendment {
                    network_policy_amendment,
                } => match network_policy_amendment.action {
                    NetworkPolicyRuleAction::Allow => RequestPermissionsResponse {
                        permissions: requested_permissions.clone(),
                        scope: PermissionGrantScope::Turn,
                        strict_auto_review: false,
                    },
                    NetworkPolicyRuleAction::Deny => RequestPermissionsResponse {
                        permissions: RequestPermissionProfile::default(),
                        scope: PermissionGrantScope::Turn,
                        strict_auto_review: false,
                    },
                },
                ReviewDecision::ApprovedMcpPolicyAmendment
                | ReviewDecision::Abort
                | ReviewDecision::Denied { .. }
                | ReviewDecision::TimedOut => RequestPermissionsResponse {
                    permissions: RequestPermissionProfile::default(),
                    scope: PermissionGrantScope::Turn,
                    strict_auto_review: false,
                },
            };
            let response = Self::normalize_request_permissions_response(
                requested_permissions,
                response,
                &context,
            );
            self.record_granted_request_permissions_for_turn(
                &response,
                &environment.selection.environment_id,
                originating_turn_state.as_ref(),
            )
            .await;
            return Some(response);
        }

        // The interactive approval event remains host-native until its public
        // app-server/TUI boundary migrates in the next stack stage.
        let Ok(native_cwd) = cwd.to_abs_path() else {
            warn!(
                cwd = %cwd,
                "request_permissions interactive approval requires a cwd native to the Codex host"
            );
            return Some(RequestPermissionsResponse {
                permissions: RequestPermissionProfile::default(),
                scope: PermissionGrantScope::Turn,
                strict_auto_review: false,
            });
        };
        let _elicitation = self.services.elicitations.register();
        let (tx_response, rx_response) = oneshot::channel();
        let prev_entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.insert_pending_request_permissions(
                        call_id.clone(),
                        PendingRequestPermissions {
                            tx_response,
                            requested_permissions: requested_permissions.clone(),
                            environment: environment.clone(),
                        },
                    )
                }
                None => None,
            }
        };
        if prev_entry.is_some() {
            warn!("Overwriting existing pending request_permissions for call_id: {call_id}");
        }

        let event = EventMsg::RequestPermissions(RequestPermissionsEvent {
            call_id: call_id.clone(),
            turn_id: turn_context.sub_id.clone(),
            environment_id: Some(environment.selection.environment_id.clone()),
            started_at_ms: now_unix_timestamp_ms(),
            reason: args.reason,
            permissions: requested_permissions,
            cwd: Some(native_cwd),
        });
        self.send_event(turn_context.as_ref(), event).await;
        tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {
                let mut active = self.active_turn.lock().await;
                if let Some(at) = active.as_mut() {
                    let mut ts = at.turn_state.lock().await;
                    let _ = ts.remove_pending_request_permissions(&call_id);
                }
                None
            }
            response = rx_response => response.ok(),
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn request_user_input(
        &self,
        turn_context: &TurnContext,
        call_id: String,
        args: RequestUserInputArgs,
    ) -> Option<RequestUserInputResponse> {
        let _elicitation = self.services.elicitations.register();
        let sub_id = turn_context.sub_id.clone();
        let (tx_response, rx_response) = oneshot::channel();
        let event_id = sub_id.clone();
        let prev_entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.insert_pending_user_input(sub_id, tx_response)
                }
                None => None,
            }
        };
        if prev_entry.is_some() {
            warn!("Overwriting existing pending user input for sub_id: {event_id}");
        }

        let event = EventMsg::RequestUserInput(RequestUserInputEvent {
            call_id,
            turn_id: turn_context.sub_id.clone(),
            questions: args.questions,
            is_blocking: args.is_blocking,
            auto_resolution_ms: args.auto_resolution_ms,
        });
        turn_context
            .turn_metadata_state
            .mark_user_input_requested_during_turn();
        self.send_event(turn_context, event).await;
        rx_response.await.ok()
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn notify_user_input_response(
        &self,
        sub_id: &str,
        response: RequestUserInputResponse,
    ) {
        let entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.remove_pending_user_input(sub_id)
                }
                None => None,
            }
        };
        match entry {
            Some(tx_response) => {
                tx_response.send(response).ok();
            }
            None => {
                warn!("No pending user input found for sub_id: {sub_id}");
            }
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn notify_request_permissions_response(
        &self,
        call_id: &str,
        response: RequestPermissionsResponse,
    ) {
        let (entry, originating_turn_state) = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    let entry = ts.remove_pending_request_permissions(call_id);
                    let originating_turn_state = entry.as_ref().map(|_| Arc::clone(&at.turn_state));
                    (entry, originating_turn_state)
                }
                None => (None, None),
            }
        };
        match entry {
            Some(entry) => {
                let sandbox_context = entry
                    .environment
                    .sandbox_context(/*additional_permissions*/ None);
                let response = if let Some(context) = sandbox_context.policy_context() {
                    Self::normalize_request_permissions_response(
                        entry.requested_permissions,
                        response,
                        &context,
                    )
                } else {
                    RequestPermissionsResponse {
                        permissions: RequestPermissionProfile::default(),
                        scope: PermissionGrantScope::Turn,
                        strict_auto_review: false,
                    }
                };
                self.record_granted_request_permissions_for_turn(
                    &response,
                    &entry.environment.selection.environment_id,
                    originating_turn_state.as_ref(),
                )
                .await;
                entry.tx_response.send(response).ok();
            }
            None => {
                warn!("No pending request_permissions found for call_id: {call_id}");
            }
        }
    }

    fn normalize_request_permissions_response(
        requested_permissions: RequestPermissionProfile,
        response: RequestPermissionsResponse,
        context: &FileSystemSandboxPolicyContext<'_>,
    ) -> RequestPermissionsResponse {
        if response.strict_auto_review && matches!(response.scope, PermissionGrantScope::Session) {
            return RequestPermissionsResponse {
                permissions: RequestPermissionProfile::default(),
                scope: PermissionGrantScope::Turn,
                strict_auto_review: false,
            };
        }

        if response.permissions.is_empty() {
            return response;
        }

        RequestPermissionsResponse {
            permissions: intersect_permission_profiles_with_context(
                requested_permissions.into(),
                response.permissions.into(),
                context,
            )
            .into(),
            scope: response.scope,
            strict_auto_review: response.strict_auto_review,
        }
    }

    async fn record_granted_request_permissions_for_turn(
        &self,
        response: &RequestPermissionsResponse,
        environment_id: &str,
        originating_turn_state: Option<&Arc<Mutex<crate::state::TurnState>>>,
    ) {
        if response.permissions.is_empty() {
            return;
        }
        match response.scope {
            PermissionGrantScope::Turn => {
                if let Some(turn_state) = originating_turn_state {
                    let mut ts = turn_state.lock().await;
                    let permissions: AdditionalPermissionProfile =
                        response.permissions.clone().into();
                    ts.record_granted_permissions(environment_id, permissions);
                    if response.strict_auto_review {
                        ts.enable_strict_auto_review();
                    }
                }
            }
            PermissionGrantScope::Session => {
                let mut state = self.state.lock().await;
                state.record_granted_permissions(
                    environment_id,
                    response.permissions.clone().into(),
                );
            }
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn reads must stay consistent with the matching turn state"
    )]
    pub(crate) async fn granted_turn_permissions(
        &self,
        environment_id: &str,
    ) -> Option<AdditionalPermissionProfile> {
        let active = self.active_turn.lock().await;
        let active = active.as_ref()?;
        let ts = active.turn_state.lock().await;
        ts.granted_permissions(environment_id)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn reads must stay consistent with the matching turn state"
    )]
    pub(crate) async fn active_turn_context_and_strict_auto_review(
        &self,
    ) -> Option<(Arc<TurnContext>, Arc<ResolvedStepSettings>, bool)> {
        let active = self.active_turn.lock().await;
        let active = active.as_ref()?;
        let task = active.task.as_ref()?;
        let turn_context = Arc::clone(&task.turn_context);
        let step_settings = turn_context.current_settings.load_full();
        let ts = active.turn_state.lock().await;
        Some((turn_context, step_settings, ts.strict_auto_review_enabled()))
    }

    pub(crate) async fn granted_session_permissions(
        &self,
        environment_id: &str,
    ) -> Option<AdditionalPermissionProfile> {
        let state = self.state.lock().await;
        state.granted_permissions(environment_id)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn notify_dynamic_tool_response(&self, call_id: &str, response: DynamicToolResponse) {
        let entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.remove_pending_dynamic_tool(call_id)
                }
                None => None,
            }
        };
        match entry {
            Some(tx_response) => {
                tx_response.send(response).ok();
            }
            None => {
                warn!("No pending dynamic tool call found for call_id: {call_id}");
            }
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn take_pending_approval(
        &self,
        approval_id: &str,
    ) -> Option<oneshot::Sender<ReviewDecision>> {
        let entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.remove_pending_approval(approval_id)
                }
                None => None,
            }
        };
        if entry.is_none() {
            warn!("No live pending approval found for call_id: {approval_id}");
        }
        entry
    }

    pub async fn notify_approval(&self, approval_id: &str, decision: ReviewDecision) -> bool {
        match self.take_pending_approval(approval_id).await {
            Some(tx_approve) => {
                tx_approve.send(decision).ok();
                true
            }
            None => false,
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn claim_pending_approval(&self, approval_id: &str) -> bool {
        let mut active = self.active_turn.lock().await;
        let Some(active) = active.as_mut() else {
            return false;
        };
        let mut turn_state = active.turn_state.lock().await;
        turn_state.claim_pending_approval(approval_id)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn release_pending_approval_claim(&self, approval_id: &str) {
        let mut active = self.active_turn.lock().await;
        let Some(active) = active.as_mut() else {
            return;
        };
        let expired = active
            .turn_state
            .lock()
            .await
            .release_pending_approval_claim(approval_id);
        if let Some(sender) = expired {
            sender.send(ReviewDecision::TimedOut).ok();
        }
    }

    pub(crate) fn response_item_create_time() -> serde_json::Number {
        let create_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        serde_json::Number::from_f64(create_time.as_secs_f64())
            .unwrap_or_else(|| serde_json::Number::from(create_time.as_secs()))
    }

    /// Adds the turn ID and creation time to newly created conversation items.
    pub(crate) fn stamp_response_item_for_history(item: &mut ResponseItem, turn_id: &str) {
        item.set_turn_id_if_missing(turn_id);
        item.set_create_time_if_missing(Self::response_item_create_time());
    }

    /// Records conversation items: append to history, persist to rollout, and
    /// notify clients observing raw response items.
    pub(crate) fn prepare_conversation_items_for_history<'a>(
        &self,
        turn_context: &TurnContext,
        items: &'a [ResponseItem],
    ) -> (Cow<'a, [ResponseItem]>, Vec<ImagePreparationMetadata>) {
        let mut items = items.to_vec();
        let image_preparation_mode = if unified_image_budget_enabled(
            &turn_context.config.features,
            turn_context.model_info(),
        ) {
            ImagePreparationMode::UnifiedBudget
        } else {
            ImagePreparationMode::DetailBased
        };
        let image_resize_notice_mode = if turn_context
            .config
            .features
            .enabled(Feature::ImageResizeNotice)
        {
            ImageResizeNoticeMode::Enabled
        } else {
            ImageResizeNoticeMode::Disabled
        };
        let image_preparations = prepare_image_response_items(
            &mut items,
            image_preparation_mode,
            image_resize_notice_mode,
        );
        prepare_audio_response_items(&mut items);
        // Most response items get their passthrough turn ID at the durable history boundary.
        for item in &mut items {
            if let ResponseItem::AgentMessage {
                id,
                internal_chat_message_metadata_passthrough,
                ..
            } = item
            {
                internal_chat_message_metadata_passthrough
                    .get_or_insert_default()
                    .turn_id = Some(turn_context.sub_id.clone());
                if id.as_ref().is_none_or(|id| {
                    !id.starts_with("amsg_")
                        || is_sub_agent_completion_context_response_item_id(id.as_str())
                }) {
                    *id = Some(ResponseItemId::new("amsg"));
                }
            }
            Self::stamp_response_item_for_history(item, &turn_context.sub_id);
        }
        let items = Cow::Owned(items);
        (
            Self::assign_missing_response_item_ids(items),
            image_preparations,
        )
    }

    fn assign_missing_response_item_ids(items: Cow<'_, [ResponseItem]>) -> Cow<'_, [ResponseItem]> {
        if items
            .iter()
            .all(|item| item.id().is_some_and(|id| !id.is_empty()))
        {
            return items;
        }
        let mut items = items;
        for item in items.to_mut() {
            Self::assign_missing_response_item_id(item);
        }
        items
    }

    fn assign_missing_response_item_id(item: &mut ResponseItem) {
        if item.id().is_some_and(|id| !id.is_empty()) {
            return;
        }
        let Some(prefix) = item.id_prefix() else {
            return;
        };
        item.set_id(Some(ResponseItemId::new(prefix)));
    }

    fn assign_missing_rollout_response_item_ids(items: &mut [RolloutItem]) {
        for item in items {
            if let RolloutItem::ResponseItem(response_item) = item {
                Self::assign_missing_response_item_id(&mut response_item.item);
            }
        }
    }

    pub(crate) fn response_item_from_user_input(&self, input: Vec<UserInput>) -> ResponseItem {
        let mut item = ResponseItem::from(ResponseInputItem::from_user_input(
            input,
            LocalImagePreparation::Defer,
        ));
        if let ResponseItem::Message {
            content,
            internal_chat_message_metadata_passthrough,
            ..
        } = &mut item
        {
            let content_item_kinds = content
                .iter()
                .map(|content| {
                    ContentItemKind(
                        match content {
                            ContentItem::InputText { .. } | ContentItem::OutputText { .. } => {
                                "user.text"
                            }
                            ContentItem::InputImage { .. } => "user.image",
                            ContentItem::InputAudio { .. } => "user.audio",
                        }
                        .to_string(),
                    )
                })
                .collect();
            *internal_chat_message_metadata_passthrough =
                Some(InternalChatMessageMetadataPassthrough {
                    content_item_kinds: Some(content_item_kinds),
                    ..Default::default()
                });
        }
        item
    }

    #[tracing::instrument(level = "trace", skip_all, fields(item_count = items.len()))]
    pub(crate) async fn record_conversation_items(
        &self,
        turn_context: &TurnContext,
        items: &[ResponseItem],
    ) {
        let (items, image_preparations) =
            self.prepare_conversation_items_for_history(turn_context, items);
        let items = items
            .into_owned()
            .into_iter()
            .map(ResponseItemEnvelope::new)
            .collect();
        self.record_prepared_conversation_items(turn_context, items, image_preparations)
            .await;
    }

    async fn record_prepared_conversation_items(
        &self,
        turn_context: &TurnContext,
        items: Vec<ResponseItemEnvelope>,
        image_preparations: Vec<ImagePreparationMetadata>,
    ) {
        let Ok(_permit) = self.durable_context_lock.acquire().await else {
            return;
        };
        let response_items = items
            .iter()
            .map(|envelope| envelope.item.clone())
            .collect::<Vec<_>>();
        let rollout_items = items
            .iter()
            .cloned()
            .map(RolloutItem::ResponseItem)
            .collect::<Vec<_>>();
        self.persist_rollout_items(&rollout_items).await;
        {
            let mut state = self.state.lock().await;
            state
                .current_time_reminder
                .note_recorded_items(&response_items);
            state
                .history
                .record_annotated_items(&items, turn_context.model_info().truncation_policy.into());
        }
        for image in image_preparations {
            self.services
                .analytics_events_client
                .track_image_preparation(ImagePreparationFact {
                    turn_id: turn_context.sub_id.clone(),
                    metadata: image,
                });
        }
        if turn_context.config.memories.disable_on_external_context
            && let Some(item) = response_items
                .iter()
                .find(|item| matches!(item, ResponseItem::FunctionCallOutput { call_id: None, .. }))
        {
            mark_thread_memory_mode_polluted_if_external_context(self, turn_context, item).await;
        }
        self.send_raw_response_items(turn_context, &response_items)
            .await;
    }

    pub(crate) async fn acquire_durable_context_permit(
        &self,
    ) -> CodexResult<tokio::sync::OwnedSemaphorePermit> {
        Arc::clone(&self.durable_context_lock)
            .acquire_owned()
            .await
            .map_err(|_| CodexErr::InternalAgentDied)
    }

    /// Records model-visible items under a completed history turn without starting model work.
    pub(crate) async fn record_history_only_conversation_items(
        &self,
        turn_context: &TurnContext,
        items: &[ResponseItem],
    ) -> Result<(), ThreadStoreError> {
        let _permit = self.durable_context_lock.acquire().await.map_err(|err| {
            ThreadStoreError::Internal {
                message: format!("failed to lock history-only recording: {err}"),
            }
        })?;
        let (items, image_preparations) =
            self.prepare_conversation_items_for_history(turn_context, items);
        let items = items.into_owned();
        let mut rollout_items = Vec::with_capacity(items.len().saturating_add(2));
        rollout_items.push(RolloutItem::EventMsg(EventMsg::TurnStarted(
            TurnStartedEvent {
                turn_id: turn_context.sub_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
                agent_queue: None,
            },
        )));
        rollout_items.extend(
            items
                .iter()
                .cloned()
                .map(ResponseItemEnvelope::new)
                .map(RolloutItem::ResponseItem),
        );
        rollout_items.push(RolloutItem::EventMsg(EventMsg::TurnComplete(
            TurnCompleteEvent {
                turn_id: turn_context.sub_id.clone(),
                last_agent_message: None,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )));
        self.try_persist_rollout_items(&rollout_items).await?;
        {
            let mut state = self.state.lock().await;
            state.current_time_reminder.note_recorded_items(&items);
            state.record_items(
                items.iter(),
                turn_context.model_info.truncation_policy.into(),
            );
        }
        for image in image_preparations {
            self.services
                .analytics_events_client
                .track_image_preparation(ImagePreparationFact {
                    turn_id: turn_context.sub_id.clone(),
                    metadata: image,
                });
        }
        self.send_raw_response_items(turn_context, &items).await;
        Ok(())
    }

    /// Records one extension contribution only after its complete rollout batch is durable.
    ///
    /// The detached commit keeps the persistence boundary cancellation-safe. Its acknowledgement
    /// remains owned by the commit and runs only after persistence and in-memory history both
    /// contain the complete contribution.
    pub(crate) async fn record_durable_context_items(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        items: Vec<ResponseItem>,
        acknowledgement: Option<TurnInputContributionAcknowledgement>,
    ) -> Result<(), ThreadStoreError> {
        self.record_durable_context_items_with_rollout_suffix(
            turn_context,
            items,
            acknowledgement,
            Vec::new(),
        )
        .await
    }

    pub(crate) async fn record_durable_context_items_with_rollout_suffix(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        items: Vec<ResponseItem>,
        acknowledgement: Option<TurnInputContributionAcknowledgement>,
        rollout_suffix: Vec<RolloutItem>,
    ) -> Result<(), ThreadStoreError> {
        let (items, image_preparations) =
            self.prepare_conversation_items_for_history(turn_context.as_ref(), &items);
        let items = items
            .into_owned()
            .into_iter()
            .map(ResponseItemEnvelope::new)
            .collect::<Vec<_>>();
        self.record_prepared_durable_context_items_with_rollout_suffix(
            turn_context,
            items,
            acknowledgement,
            Vec::new(),
            rollout_suffix,
            image_preparations,
        )
        .await
    }

    async fn record_prepared_durable_context_items_with_rollout_suffix(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        mut items: Vec<ResponseItemEnvelope>,
        acknowledgement: Option<TurnInputContributionAcknowledgement>,
        rollout_prefix: Vec<RolloutItem>,
        rollout_suffix: Vec<RolloutItem>,
        image_preparations: Vec<ImagePreparationMetadata>,
    ) -> Result<(), ThreadStoreError> {
        let sess = Arc::clone(self);
        tokio::spawn(async move {
            let _permit = sess.durable_context_lock.acquire().await.map_err(|err| {
                ThreadStoreError::Internal {
                    message: format!("failed to lock durable context recording: {err}"),
                }
            })?;
            let mut rollout_items = Vec::with_capacity(
                rollout_prefix
                    .len()
                    .saturating_add(items.len())
                    .saturating_add(rollout_suffix.len()),
            );
            rollout_items.extend(rollout_prefix);
            for item in &items {
                if item
                    .id()
                    .is_some_and(|id| is_sub_agent_completion_context_response_item_id(id.as_str()))
                {
                    rollout_items.push(RolloutItem::InterAgentCommunicationMetadata {
                        trigger_turn: false,
                    });
                }
                rollout_items.push(RolloutItem::ResponseItem(item.clone()));
            }
            rollout_items.extend(rollout_suffix.iter().cloned());
            let response_delivery_id = match items.as_slice() {
                [item] if !rollout_suffix.is_empty() => item.id().cloned(),
                [] | [_] | [_, _, ..] => None,
            };
            let persisted_delivery = match response_delivery_id.as_ref() {
                Some(response_item_id) => Some(
                    sess.persisted_response_observation_delivery(response_item_id)
                        .await?,
                ),
                None => None,
            };
            // Observation delivery appends the response and its committed-state suffix as one
            // canonical batch. A post-commit store error can hide that success from the watcher,
            // so retry repairs only a missing suffix and reuses the persisted response item.
            let persisted_completion_item = match persisted_delivery.as_ref() {
                Some(delivery) => delivery.response_item.clone(),
                None => match items.as_slice() {
                    [item] => match item.id() {
                        Some(response_item_id)
                            if is_sub_agent_completion_context_response_item_id(
                                response_item_id.as_str(),
                            ) =>
                        {
                            sess.persisted_sub_agent_completion_context_item(response_item_id)
                                .await?
                        }
                        Some(_) | None => None,
                    },
                    [] | [_, _, ..] => None,
                },
            };
            if let Some(persisted_completion_item) = persisted_completion_item {
                items = vec![persisted_completion_item.into()];
                if !rollout_suffix.is_empty()
                    && !persisted_delivery
                        .as_ref()
                        .is_some_and(|delivery| delivery.committed)
                    && let Some(live_thread) = sess.live_thread()
                {
                    live_thread
                        .append_items_and_flush_canonical(&rollout_suffix)
                        .await?;
                }
            } else if !rollout_suffix.is_empty()
                && let Some(live_thread) = sess.live_thread()
            {
                live_thread
                    .append_items_and_flush_canonical(&rollout_items)
                    .await?;
            } else {
                sess.try_persist_rollout_items(&rollout_items).await?;
            }
            let response_items = items
                .iter()
                .map(|envelope| envelope.item.clone())
                .collect::<Vec<_>>();
            let response_already_recorded = {
                let mut state = sess.state.lock().await;
                let already_recorded = response_delivery_id
                    .as_ref()
                    .is_some_and(|id| state.history.raw_items().any(|item| item.id() == Some(id)));
                if !already_recorded {
                    state
                        .current_time_reminder
                        .note_recorded_items(&response_items);
                    state.history.record_annotated_items(
                        &items,
                        turn_context.model_info.truncation_policy.into(),
                    );
                }
                already_recorded
            };
            if !response_already_recorded {
                sess.send_raw_response_items(turn_context.as_ref(), &response_items)
                    .await;
            }
            for image in image_preparations {
                sess.services
                    .analytics_events_client
                    .track_image_preparation(ImagePreparationFact {
                        turn_id: turn_context.sub_id.clone(),
                        metadata: image,
                    });
            }
            if let Some(acknowledgement) = acknowledgement {
                acknowledgement.acknowledge();
            }
            Ok(())
        })
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("durable context recording task failed: {err}"),
        })?
    }

    pub(crate) async fn record_step_world_state_if_changed(
        &self,
        previous_world_state: &Arc<WorldState>,
        step_context: &step_context::StepContext,
    ) -> CodexResult<Arc<WorldState>> {
        let turn_context = step_context.turn.as_ref();
        // Render model-visible state from the same step used to build and run tools.
        let world_state = Arc::new(self.build_world_state_for_step(step_context).await?);
        // Derive the model update and persisted patch from the same two snapshots.
        let previous_snapshot = previous_world_state.snapshot();
        let world_state_snapshot = world_state.snapshot();
        let world_state_item = world_state_snapshot
            .merge_patch_from(&previous_snapshot)
            .map(WorldStateItem::patch);
        let items = crate::context_manager::updates::merge_contextual_fragments(
            world_state.render_diff(&previous_snapshot),
        );
        if !items.is_empty() {
            self.record_conversation_items(turn_context, &items).await;
        }

        // ContextManager remembers this for later turns; run_turn owns the live value.
        self.state
            .lock()
            .await
            .history
            .set_world_state_baseline(world_state_snapshot);
        // Record the patch after the context it describes is present in model history.
        if let Some(world_state_item) = world_state_item {
            self.persist_rollout_items(&[RolloutItem::WorldState(world_state_item)])
                .await;
        }
        Ok(world_state)
    }

    /// Retains the step captured for execution.
    pub(crate) async fn set_last_known_step_context(&self, step_context: &Arc<StepContext>) {
        let turn_state = {
            let active_turn = self.active_turn.lock().await;
            active_turn.as_ref().and_then(|active_turn| {
                active_turn
                    .task
                    .as_ref()
                    .filter(|task| task.turn_context.sub_id == step_context.turn.sub_id)
                    .map(|_| Arc::clone(&active_turn.turn_state))
            })
        };
        if let Some(turn_state) = turn_state {
            turn_state.lock().await.last_known_step_context = Some(Arc::clone(step_context));
        }
    }

    /// Captures one request-scoped view of dynamic state and retains it for the active turn.
    ///
    /// This may refresh filesystem-derived state. Normal turns should call it only from
    /// `run_turn` and pass the result down; standalone request or history boundaries may capture
    /// their own step. Use speculative capture for a step that may not execute.
    pub(crate) async fn capture_step_context(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        cancellation_token: &CancellationToken,
    ) -> CodexResult<Arc<StepContext>> {
        self.capture_step_context_with_required_mcp_servers(
            turn_context,
            cancellation_token,
            /*required_servers*/ &[],
        )
        .await
    }

    pub(crate) async fn capture_step_context_with_required_mcp_servers(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        cancellation_token: &CancellationToken,
        required_servers: &[String],
    ) -> CodexResult<Arc<StepContext>> {
        let step_context = self
            .capture_step_context_inner(turn_context, cancellation_token, required_servers)
            .await?;
        self.set_last_known_step_context(&step_context).await;
        Ok(step_context)
    }

    /// Prepares a candidate step without replacing the active turn's retained context.
    /// The caller must retain it explicitly if it is selected for execution.
    async fn capture_speculative_step_context(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        cancellation_token: &CancellationToken,
    ) -> CodexResult<Arc<StepContext>> {
        self.capture_step_context_inner(
            turn_context,
            cancellation_token,
            /*required_servers*/ &[],
        )
        .await
    }

    #[tracing::instrument(name = "step_context.capture", level = "info", skip_all)]
    async fn capture_step_context_inner(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        cancellation_token: &CancellationToken,
        required_servers: &[String],
    ) -> CodexResult<Arc<StepContext>> {
        // Capture once before asynchronous planning; all request consumers
        // retain this immutable settings version even if the turn is updated.
        let mut settings = turn_context.current_settings.load_full();
        if matches!(
            turn_context.session_source,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        ) {
            let root_service_tier = self.services.agent_control.root_service_tier();
            if settings.selected().service_tier != root_service_tier {
                let mut selected = settings.selected().clone();
                selected.service_tier = root_service_tier;
                let mut inherited_settings = ResolvedStepSettings::new(
                    Arc::new(selected),
                    Arc::clone(&settings.model_info),
                    self.features.enabled(Feature::FastMode),
                );
                inherited_settings.mcp_approvals_reviewer_override =
                    settings.mcp_approvals_reviewer_override;
                settings = Arc::new(inherited_settings);
            }
        }
        let token_budget = token_budget::resolve_token_budget(
            turn_context.configured_token_budget.as_ref(),
            turn_context.use_model_token_budget_defaults,
            settings.model_info.as_ref(),
        );
        let session_telemetry = settings.telemetry(&turn_context.session_telemetry);
        // Keep selections fixed for the turn while allowing their startup work to finish.
        let environments = turn_context.environments.refresh_readiness();
        self.services
            .agents_md_manager
            .refresh(&turn_context.config, &environments)
            .await?;
        let loaded_agents_md = self.services.agents_md_manager.get_loaded().await;
        let selected_capability_roots = self
            .resolve_selected_capability_roots_for_step(&environments)
            .await;
        let ready_selected_capability_roots =
            Self::ready_selected_capability_roots(&selected_capability_roots);
        let executor_capability_discovery = self
            .executor_capability_discovery_for_step(
                &turn_context.config,
                &ready_selected_capability_roots,
                &environments,
            )
            .or_cancel(cancellation_token)
            .await?;
        let extension_data = codex_extension_api::ExtensionData::new(turn_context.sub_id.clone());
        extension_data.insert(selected_capability_roots.clone());
        if let Some(discovery) = &executor_capability_discovery {
            extension_data.insert(discovery.as_ref().clone());
            if !discovery.sandbox_contexts().is_empty() {
                extension_data.insert(discovery.sandbox_contexts().clone());
            }
        } else if !environments
            .permission_profile_or_else(|| turn_context.permission_profile())
            .file_system_sandbox_policy()
            .has_full_disk_read_access()
        {
            let sandbox_contexts = environments
                .turn_environments()
                .map(|environment| {
                    (
                        environment.selection.environment_id.clone(),
                        environment.sandbox_context(/*additional_permissions*/ None),
                    )
                })
                .collect::<HashMap<_, _>>();
            extension_data.insert(sandbox_contexts);
        }
        let (mcp, prepared_recommendations) = async {
            tokio::join!(
                self.mcp_runtime_for_step(
                    turn_context.as_ref(),
                    &selected_capability_roots,
                    required_servers,
                ),
                turn::prepare_tool_recommendations(self.as_ref(), turn_context.as_ref()),
            )
        }
        .or_cancel(cancellation_token)
        .await?;
        let mut selected_plugins = self
            .services
            .thread_extension_data
            .get::<codex_extension_api::SelectedPluginSnapshot>()
            .map(|snapshot| snapshot.as_ref().clone())
            .unwrap_or_default();
        selected_plugins.plugins.retain(|plugin| {
            ready_selected_capability_roots
                .iter()
                .any(|root| root.id == plugin.selected_root_id)
        });
        extension_data.insert(selected_plugins.clone());
        turn_context.extension_data.insert(selected_plugins);
        // Tool availability still follows the admitted turn; the async message
        // description comes from the captured step model.
        let tool_router = turn::built_tools(
            self.as_ref(),
            turn_context.as_ref(),
            // TODO(CDXENT-441): use the step scoped model
            turn_context.model_info(),
            settings.model_info.model_messages.as_ref(),
            &environments,
            &mcp,
            &extension_data,
            prepared_recommendations,
        )
        .or_cancel(cancellation_token)
        .await??;
        // Publish inventory after planning rather than during finalization, so constructing
        // additional candidate plans cannot overwrite turn-wide metadata.
        if turn_context
            .config
            .tool_registry
            .turn_metadata_includes_tool_info
            && turn_context.model_info().use_responses_lite
        {
            turn_context.turn_metadata_state.set_tool_namespaces_info(
                tool_router
                    .tool_namespaces_info()
                    .cloned()
                    .unwrap_or_default(),
            );
        }
        Ok(Arc::new(StepContext {
            settings,
            token_budget,
            session_telemetry,
            turn: turn_context,
            environments,
            selected_capability_roots,
            executor_capability_discovery,
            mcp,
            tool_router,
            loaded_agents_md,
        }))
    }

    pub(crate) async fn record_conversation_items_silently(
        &self,
        turn_context: &TurnContext,
        items: &[ResponseItem],
    ) {
        self.record_into_history(items, turn_context).await;
        let rollout_items = items
            .iter()
            .cloned()
            .map(|item| RolloutItem::ResponseItem(item.into()))
            .collect::<Vec<_>>();
        self.persist_rollout_items(&rollout_items).await;
    }

    /// Append ResponseItems to the in-memory conversation history only.
    pub(crate) async fn record_into_history(
        &self,
        items: &[ResponseItem],
        turn_context: &TurnContext,
    ) {
        let mut state = self.state.lock().await;
        state.record_items(
            items.iter(),
            turn_context.model_info.truncation_policy.into(),
        );
    }

    async fn maybe_warn_on_server_model_mismatch(
        self: &Arc<Self>,
        step_context: &StepContext,
        server_model: String,
    ) -> bool {
        let turn_context = &step_context.turn;
        let requested_model = step_context.settings.model_info.slug.clone();
        let server_model_normalized = server_model.to_ascii_lowercase();
        let requested_model_normalized = requested_model.to_ascii_lowercase();
        if server_model_normalized == requested_model_normalized {
            info!("server reported model {server_model} (matches requested model)");
            return false;
        }

        warn!("server reported model {server_model} while requested model was {requested_model}");

        let warning_message = format!(
            "Your account was flagged for potentially high-risk cyber activity and this request was routed to gpt-5.2 as a fallback. To regain access to gpt-5.3-codex, apply for trusted access: {CYBER_VERIFY_URL} or learn more: {CYBER_SAFETY_URL}"
        );

        self.send_event(
            turn_context,
            EventMsg::ModelReroute(ModelRerouteEvent {
                from_model: requested_model.clone(),
                to_model: server_model.clone(),
                reason: ModelRerouteReason::HighRiskCyberActivity,
            }),
        )
        .await;

        self.send_event(
            turn_context,
            EventMsg::Warning(WarningEvent {
                message: warning_message.clone(),
            }),
        )
        .await;
        true
    }

    pub(crate) async fn emit_model_verification(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        verifications: Vec<ModelVerification>,
    ) {
        self.send_event(
            turn_context,
            EventMsg::ModelVerification(ModelVerificationEvent { verifications }),
        )
        .await;
    }

    pub(crate) async fn emit_turn_moderation_metadata(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        metadata: TurnModerationMetadataEvent,
    ) {
        self.send_event(turn_context, EventMsg::TurnModerationMetadata(metadata))
            .await;
    }

    #[cfg(test)]
    pub(crate) async fn replace_history(
        &self,
        items: Vec<ResponseItem>,
        reference_context_item: Option<TurnContextItem>,
    ) {
        let mut state = self.state.lock().await;
        state.replace_history(items, reference_context_item);
    }

    pub(crate) async fn replace_compacted_history(
        self: &Arc<Self>,
        _turn_context: Arc<TurnContext>,
        items: Vec<ResponseItemEnvelope>,
        reference_context_item: Option<TurnContextItem>,
        world_state_baseline: Option<Arc<WorldState>>,
        metadata: CompactedHistoryMetadata,
    ) -> CodexResult<Vec<ResponseItemEnvelope>> {
        let sess = Arc::clone(self);
        tokio::spawn(async move {
            // Wait for accepted updates to finish persisting, then keep later updates from
            // overtaking the current settings snapshot while its checkpoint is written.
            let _settings_guard = thread_settings::acquire_persistence_lock(&sess).await;
            let _response_observation_transaction = sess
                .services
                .agent_control
                .acquire_response_observation_transaction(sess.presentation_id())
                .await;
            let _permit = sess.durable_context_lock.acquire().await.map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to lock compacted history installation: {err}"
                ))
            })?;
            let mut post_compaction_contributions = Vec::new();
            let mut post_compaction_items = Vec::new();
            for contributor in sess.services.extensions.context_contributors() {
                let mut contribution = contributor
                    .contribute_post_compaction_context(
                        &sess.services.session_extension_data,
                        &sess.services.thread_extension_data,
                    )
                    .await;
                post_compaction_items.extend(contribution.take_items());
                post_compaction_contributions.push(contribution);
            }
            let (mut history_before_compaction, latest_token_usage_record) = {
                let state = sess.state.lock().await;
                (
                    state.history.clone(),
                    state.latest_token_usage_record.clone(),
                )
            };
            let previous_history = history_before_compaction.annotated_items().to_vec();
            let mut final_items = {
                let final_items = crate::compact::preserve_annotated_mcp_server_use_context_items(
                    items,
                    &previous_history,
                );
                let final_items = crate::compact::preserve_annotated_promoted_skills_inventory_item(
                    final_items,
                    &previous_history,
                );
                let mut final_items =
                    crate::compact::insert_annotated_post_compaction_context_items(
                        final_items,
                        post_compaction_items,
                    );
                for envelope in &mut final_items {
                    Self::assign_missing_response_item_id(&mut envelope.item);
                }
                final_items
            };
            let mut raw_items = final_items
                .iter()
                .map(|envelope| envelope.item.clone())
                .collect::<Vec<_>>();
            let media_sanitization = crate::context::sanitize_compacted_media(&mut raw_items);
            for (envelope, item) in final_items.iter_mut().zip(raw_items) {
                envelope.item = item;
            }
            if media_sanitization.changed() {
                info!(
                    omitted_image_count = media_sanitization.omitted_image_count,
                    omitted_inline_media_bytes = media_sanitization.omitted_inline_media_bytes,
                    "removed media while installing compacted history"
                );
            }
            // Derive the checkpoint from the same transition that will be installed below while
            // retaining the persistence-first failure boundary. On the first compaction this
            // initializes review history from the complete pre-compaction conversation.
            history_before_compaction.replace_compacted(final_items.clone());
            let guardian_history = history_before_compaction.guardian_history_checkpoint();

            let world_state_snapshot =
                world_state_baseline.map(|world_state| world_state.snapshot());
            let compacted_item = CompactedItem {
                message: metadata.message,
                replacement_history: Some(final_items.clone()),
                guardian_history,
                mcp_resource_origins: sess.services.mcp_runtime.resource_origin_checkpoint(),
                compaction_summary_tokens: metadata.compaction_summary_tokens,
                window_number: Some(metadata.window_number),
                first_window_id: Some(metadata.window_ids.first_window_id.to_string()),
                previous_window_id: metadata
                    .window_ids
                    .previous_window_id
                    .map(|id| id.to_string()),
                window_id: Some(metadata.window_ids.window_id.to_string()),
                compaction_response_id: metadata.compaction_response_id,
                latest_token_usage_record,
                replacement_history_media_sanitized_prefix_len: Some(
                    u64::try_from(final_items.len()).unwrap_or(u64::MAX),
                ),
                replacement_history_media_repair: false,
            };

            let mut rollout_items = vec![RolloutItem::Compacted(compacted_item)];
            rollout_items.extend(
                sess.services
                    .agent_control
                    .response_observation_snapshots_for_parent(sess.presentation_id())
                    .into_iter()
                    .map(RolloutItem::AgentResponseObservation),
            );
            // Persist the baseline after the replacement history that established it.
            if let Some(snapshot) = world_state_snapshot.as_ref() {
                rollout_items.push(RolloutItem::WorldState(WorldStateItem::full(
                    snapshot.clone().into_object(),
                )));
            }
            if let Some(turn_context_item) = reference_context_item.as_ref() {
                rollout_items.push(RolloutItem::TurnContext(turn_context_item.clone()));
            }
            // The frozen turn context must not override current settings in persisted metadata.
            rollout_items.push(RolloutItem::EventMsg(
                thread_settings::applied_event(&sess).await,
            ));
            if let Some(live_thread) = sess.live_thread() {
                live_thread
                    .append_items_and_flush_canonical(&rollout_items)
                    .await
                    .map_err(|err| {
                        CodexErr::Fatal(format!("failed to persist compacted history: {err}"))
                    })?;
            }
            {
                let mut state = sess.state.lock().await;
                state.replace_annotated_history(
                    final_items.clone(),
                    reference_context_item,
                    HistoryReplacement::Compaction,
                );
                state
                    .history
                    .set_compacted_prefix_len(Some(final_items.len()));
                if let Some(snapshot) = world_state_snapshot {
                    state.history.set_world_state_baseline(snapshot);
                }
                state.queue_pending_session_start_source(codex_hooks::SessionStartSource::Compact);
            }
            drop(post_compaction_contributions);
            Ok(final_items)
        })
        .await?
    }

    pub fn enabled(&self, feature: Feature) -> bool {
        self.features.enabled(feature)
    }

    pub(crate) fn features(&self) -> ManagedFeatures {
        self.features.clone()
    }

    pub(crate) async fn collaboration_mode(&self) -> CollaborationMode {
        let state = self.state.lock().await;
        state
            .session_configuration
            .step_settings
            .collaboration_mode
            .clone()
    }

    pub(crate) fn multi_agent_version(&self) -> Option<MultiAgentVersion> {
        self.multi_agent_version.get().copied()
    }

    pub(crate) fn set_multi_agent_version_if_unset(
        &self,
        multi_agent_version: MultiAgentVersion,
    ) -> MultiAgentVersion {
        *self.multi_agent_version.get_or_init(|| multi_agent_version)
    }

    pub(crate) fn resolve_multi_agent_version_for_model(
        &self,
        model_info: &ModelInfo,
        config: &Config,
    ) -> MultiAgentVersion {
        if let Some(multi_agent_version) = self.multi_agent_version() {
            return config.multi_agent_version_for_model(Some(multi_agent_version));
        }

        let selected = config.multi_agent_version_for_model(model_info.multi_agent_version);

        self.set_multi_agent_version_if_unset(selected)
    }

    #[tracing::instrument(level = "trace", skip_all, fields(item_count = items.len()))]
    async fn send_raw_response_items(&self, turn_context: &TurnContext, items: &[ResponseItem]) {
        for item in items {
            self.send_event(
                turn_context,
                EventMsg::RawResponseItem(RawResponseItemEvent { item: item.clone() }),
            )
            .await;
        }
    }

    async fn build_turn_context_contribution_items(
        &self,
        step_context: &StepContext,
    ) -> Vec<ResponseItem> {
        let turn_context = step_context.turn.as_ref();
        let mut developer_sections = Vec::new();
        let context_contributors = self.services.extensions.context_contributors().to_vec();

        for contributor in &context_contributors {
            for fragment in contributor
                .contribute_turn_context(TurnContextContributionInput {
                    thread_id: self.thread_id(),
                    turn_id: turn_context.sub_id.as_str(),
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                    turn_store: turn_context.extension_data.as_ref(),
                    model_context_window: turn_context.model_context_window(),
                })
                .await
            {
                developer_sections.push(fragment.into());
            }
        }

        crate::context_manager::updates::build_rendered_message(developer_sections)
            .into_iter()
            .collect()
    }

    pub(crate) async fn build_initial_context_with_world_state(
        &self,
        turn_context: &TurnContext,
        world_state: &WorldState,
    ) -> Vec<ResponseItem> {
        let compact_subagent = turn_context.is_compact_subagent();
        let mut developer_sections = Vec::<RenderedFragment>::with_capacity(8);
        let mut contextual_user_sections = Vec::<RenderedFragment>::with_capacity(2);
        let mut separate_developer_sections = Vec::<RenderedFragment>::new();
        let mut context_window_hints = Vec::new();
        let (session_source, auto_compact_window_ids) = {
            let state = self.state.lock().await;
            (
                state.session_configuration.session_source.clone(),
                state.auto_compact_window_ids(),
            )
        };
        let separate_guardian_developer_message =
            crate::guardian::is_basic_session_source(&session_source);
        // Keep the guardian policy prompt out of the aggregated developer bundle so it
        // stays isolated as its own top-level developer message for guardian subagents.
        if !separate_guardian_developer_message
            && let Some(developer_instructions) = turn_context.developer_instructions.as_deref()
            && !developer_instructions.is_empty()
        {
            developer_sections
                .push(DeveloperInstructions::new(developer_instructions).render_fragment());
        }
        if !compact_subagent {
            let loaded_plugins = self
                .services
                .plugins_manager
                .plugins_for_config(&turn_context.config.plugins_config_input())
                .await;
            let recommended_plugin_candidates = if turn_context
                .config
                .features
                .plugin_recommendations_enabled()
            {
                let auth = self.services.auth_manager.auth().await;
                let plugins_config = turn_context.config.plugins_config_input();
                self.services
                    .plugins_manager
                    .recommended_plugin_candidates_for_config(RecommendedPluginCandidatesInput {
                        plugins_config: &plugins_config,
                        loaded_plugins: &loaded_plugins,
                        auth: auth.as_ref(),
                        disabled_tools: &turn_context.config.tool_suggest.disabled_tools,
                        app_server_client_name: turn_context.app_server_client_name.as_deref(),
                    })
                    .await
            } else {
                None
            };
            if let Some(recommended_plugins) = recommended_plugin_candidates
                .as_deref()
                .and_then(RecommendedPluginsInstructions::from_plugins)
            {
                contextual_user_sections.push(recommended_plugins.render_fragment());
            }
            let context_contributors = self.services.extensions.context_contributors().to_vec();
            for contributor in &context_contributors {
                for fragment in contributor
                    .contribute_thread_context(
                        &self.services.session_extension_data,
                        &self.services.thread_extension_data,
                    )
                    .await
                {
                    match fragment.slot() {
                        PromptSlot::ContextWindow => {
                            context_window_hints.push(fragment.text().to_string());
                        }
                        PromptSlot::DeveloperPolicy | PromptSlot::DeveloperCapabilities => {
                            developer_sections.push(fragment.into());
                        }
                    }
                }
            }
            for contributor in &context_contributors {
                for fragment in contributor
                    .contribute_turn_context(TurnContextContributionInput {
                        thread_id: self.thread_id(),
                        turn_id: turn_context.sub_id.as_str(),
                        session_store: &self.services.session_extension_data,
                        thread_store: &self.services.thread_extension_data,
                        turn_store: turn_context.extension_data.as_ref(),
                        model_context_window: turn_context.model_context_window(),
                    })
                    .await
                {
                    developer_sections.push(fragment.into());
                }
            }
        }
        // This is full-context metadata. Steady-state context diffs should not re-emit it.
        if turn_context.config.features.enabled(Feature::TokenBudget)
            && turn_context.model_context_window().is_some()
        {
            // Keep the legacy bridge hint when native Notes is disabled. A failed
            // native request must not fall back to the bridge.
            if !turn_context
                .config
                .token_budget
                .as_ref()
                .is_some_and(|config| config.use_history_notes_extension)
                && let Some(mcp_result) = self
                    .services
                    .mcp_runtime
                    .latest_call_tool(
                        "notes",
                        "thread_hint",
                        /*environment_id*/ None,
                        /*arguments*/ None,
                        Some(serde_json::json!({
                            "threadId": self.thread_id().to_string(),
                        })),
                        /*requested_timeout*/ None,
                        /*wait_for_server*/ true,
                    )
                    .await
                    .ok()
                    .and_then(|result| {
                        let text = result
                            .content
                            .iter()
                            .filter_map(|content| {
                                content.get("text").and_then(serde_json::Value::as_str)
                            })
                            .filter(|text| !text.is_empty())
                            .collect::<Vec<_>>()
                            .join("\n");
                        (!text.is_empty()).then_some(text)
                    })
            {
                context_window_hints.push(mcp_result);
            }
            separate_developer_sections.push(
                crate::context::TokenBudgetContext::new(
                    session_source
                        .get_agent_path()
                        .unwrap_or_else(codex_protocol::AgentPath::root),
                    auto_compact_window_ids.first_window_id,
                    auto_compact_window_ids.previous_window_id,
                    auto_compact_window_ids.window_id,
                    (!context_window_hints.is_empty()).then(|| context_window_hints.join("\n")),
                )
                .render_fragment(),
            );
        }
        // Render the active mode after the usage hint so it can override that hint.
        let mut initial_multi_agent_mode = None;
        let mut managed_developer_instructions = None;
        for fragment in world_state.render_full() {
            match fragment.role() {
                "developer"
                    if fragment.markers().0 == ModelSwitchInstructions::type_markers().0 =>
                {
                    // New-model instructions must precede the rest of the developer context.
                    developer_sections.insert(0, fragment.render_fragment());
                }
                "developer" if fragment.markers().0 == MULTI_AGENT_MODE_OPEN_TAG => {
                    initial_multi_agent_mode = Some(fragment);
                }
                "developer"
                    if fragment.markers().0 == ManagedDeveloperInstructions::type_markers().0 =>
                {
                    managed_developer_instructions = Some(fragment);
                }
                "developer"
                    if fragment.markers().0 == MultiAgentRoleInstructions::type_markers().0 =>
                {
                    separate_developer_sections.push(fragment.render_fragment());
                }
                "developer"
                    if fragment.requires_separate_message() && fragment.markers().0.is_empty() =>
                {
                    separate_developer_sections.push(fragment.render_fragment());
                }
                "developer" => developer_sections.push(fragment.render_fragment()),
                "user" => contextual_user_sections.push(fragment.render_fragment()),
                _ => {}
            }
        }

        let mut items = Vec::with_capacity(4);
        if let Some(developer_message) =
            crate::context_manager::updates::build_rendered_message(developer_sections)
        {
            items.push(developer_message);
        }
        for section in separate_developer_sections {
            if let Some(developer_message) =
                crate::context_manager::updates::build_rendered_message(vec![section])
            {
                items.push(developer_message);
            }
        }
        if let Some(initial_multi_agent_mode) = initial_multi_agent_mode
            && let Some(message) = crate::context_manager::updates::build_rendered_message(vec![
                initial_multi_agent_mode.render_fragment(),
            ])
        {
            items.push(message);
        }
        if let Some(contextual_user_message) =
            crate::context_manager::updates::build_rendered_message(contextual_user_sections)
        {
            items.push(contextual_user_message);
        }
        // Emit the guardian policy prompt as a separate developer item so the guardian
        // subagent sees a distinct, easy-to-audit instruction block.
        if separate_guardian_developer_message
            && let Some(developer_instructions) = turn_context.developer_instructions.as_deref()
            && !developer_instructions.is_empty()
            && let Some(guardian_developer_message) =
                crate::context_manager::updates::build_rendered_message(vec![
                    GuardianPolicy::new(developer_instructions).render_fragment(),
                ])
        {
            items.push(guardian_developer_message);
        }
        if let Some(managed_developer_instructions) = managed_developer_instructions
            && let Some(message) = crate::context_manager::updates::build_rendered_message(vec![
                managed_developer_instructions.render_fragment(),
            ])
        {
            items.push(message);
        }
        // New context windows and compaction install these items directly into replacement history.
        for item in &mut items {
            item.set_turn_id_if_missing(&turn_context.sub_id);
        }
        items
    }

    #[tracing::instrument(level = "trace", skip_all, fields(item_count = items.len()))]
    pub(crate) async fn persist_rollout_items(&self, items: &[RolloutItem]) {
        if let Err(e) = self.try_persist_rollout_items(items).await {
            error!("failed to record rollout items: {e:#}");
        }
    }

    async fn try_persist_rollout_items(
        &self,
        items: &[RolloutItem],
    ) -> Result<(), ThreadStoreError> {
        if let Some(live_thread) = self.live_thread() {
            live_thread.append_items(items).await?;
        }
        Ok(())
    }

    pub(crate) async fn clone_history(&self) -> ContextManager {
        let state = self.state.lock().await;
        state.clone_history()
    }

    pub(crate) async fn conversation_history_snapshot(
        &self,
    ) -> Arc<dyn ConversationHistorySnapshot> {
        let state = self.state.lock().await;
        state.history.conversation_history_snapshot()
    }

    pub(crate) async fn current_window_id(&self) -> String {
        self.current_window().await.0
    }

    pub(crate) async fn current_window(&self) -> (String, u64, Uuid) {
        let state = self.state.lock().await;
        let thread_id = self.thread_id;
        let window_number = state.auto_compact_window_number();
        let context_window_id = state.auto_compact_window_ids().window_id;
        (
            format!("{thread_id}:{window_number}"),
            window_number,
            context_window_id,
        )
    }

    pub(crate) async fn advance_auto_compact_window(&self) -> (u64, AutoCompactWindowIds) {
        let mut state = self.state.lock().await;
        state.advance_auto_compact_window()
    }

    pub(crate) async fn request_new_context_window(&self) {
        let mut state = self.state.lock().await;
        state.request_new_context_window();
    }

    pub(crate) async fn take_new_context_window_request(&self) -> bool {
        let mut state = self.state.lock().await;
        state.take_new_context_window_request()
    }

    pub(crate) async fn start_new_context_window(
        self: &Arc<Self>,
        step_context: &StepContext,
        world_state: Arc<WorldState>,
    ) -> CodexResult<u64> {
        let turn_context = Arc::clone(&step_context.turn);
        let retained_client_developer_messages =
            if self.enabled(Feature::RetainClientDeveloperMessages) {
                let history = self.clone_history().await;
                crate::compact_remote_v2::truncate_retained_messages_for_remote_compaction(
                    history
                        .annotated_items()
                        .iter()
                        .filter(|item| {
                            crate::compact_remote_v2::is_client_authored_developer_message(item)
                        })
                        .cloned()
                        .collect(),
                    crate::compact_remote_v2::RETAINED_MESSAGE_TOKEN_BUDGET,
                )
            } else {
                Vec::new()
            };
        let window = {
            let mut state = self.state.lock().await;
            state.start_new_context_window()
        };
        let (window_number, window_ids) = window;
        let context_items = self
            .build_initial_context_with_world_state(turn_context.as_ref(), world_state.as_ref())
            .await
            .into_iter()
            .map(ResponseItemEnvelope::new)
            .chain(retained_client_developer_messages)
            .collect();
        let turn_context_item = turn_context.to_turn_context_item();
        self.replace_compacted_history(
            Arc::clone(&turn_context),
            context_items,
            Some(turn_context_item),
            Some(world_state),
            CompactedHistoryMetadata {
                message: String::new(),
                compaction_summary_tokens: None,
                window_number,
                window_ids,
                compaction_response_id: None,
            },
        )
        .await?;
        self.recompute_token_usage(turn_context.as_ref()).await;
        Ok(window_number)
    }

    pub(crate) async fn reference_context_item(&self) -> Option<TurnContextItem> {
        let state = self.state.lock().await;
        state.reference_context_item()
    }

    /// Persist the latest turn context snapshot for the first real user turn and for
    /// steady-state turns that emit model-visible context updates.
    ///
    /// When the reference snapshot is missing, this injects full initial context. Otherwise, it
    /// emits only context diffs.
    ///
    /// If full context is injected and a model switch occurred, this prepends the
    /// `<model_switch>` developer message so model-specific instructions are not lost.
    ///
    /// This is the normal runtime path that establishes a new `reference_context_item`.
    /// Mid-turn compaction is the other path that can re-establish that reference when it
    /// reinjects full initial context into replacement history. Live world-state changes may
    /// independently advance their in-memory baseline within a turn.
    #[instrument(level = "trace", skip_all)]
    pub(crate) async fn record_context_updates_and_set_reference_context_item(
        &self,
        step_context: &StepContext,
    ) -> CodexResult<Arc<WorldState>> {
        let turn_context = step_context.turn.as_ref();
        let reference_context_item = {
            let state = self.state.lock().await;
            state.reference_context_item()
        };
        let turn_context_item = turn_context.to_turn_context_item();
        let turn_context_changed = reference_context_item.as_ref() != Some(&turn_context_item);
        let should_inject_full_context = reference_context_item.is_none();
        let world_state = Arc::new(self.build_world_state_for_step(step_context).await?);
        // Full initial context resets the baseline; later turns persist only its changes.
        let (mut context_items, world_state_item) = if should_inject_full_context {
            let context_items = self
                .build_initial_context_with_world_state(turn_context, world_state.as_ref())
                .await;
            let snapshot = world_state.snapshot();
            self.state
                .lock()
                .await
                .history
                .set_world_state_baseline(snapshot.clone());
            (
                context_items,
                Some(WorldStateItem::full(snapshot.into_object())),
            )
        } else {
            let (world_state_items, world_state_item) = {
                let mut state = self.state.lock().await;
                let (fragments, rollout_item) =
                    state.history.update_world_state(world_state.as_ref());
                (
                    crate::context_manager::updates::merge_contextual_fragments(fragments),
                    rollout_item,
                )
            };
            (world_state_items, world_state_item)
        };
        if !should_inject_full_context && turn_context_changed {
            context_items.extend(
                self.build_turn_context_contribution_items(step_context)
                    .await,
            );
        }
        // A snapshot can change without producing model-visible or TurnContext updates.
        let only_world_state_changed = !turn_context_changed && context_items.is_empty();
        if only_world_state_changed && world_state_item.is_none() {
            return Ok(world_state);
        }
        if !context_items.is_empty() {
            self.record_conversation_items(turn_context, &context_items)
                .await;
        }
        // Persist state only after any model-visible context generated from it.
        if let Some(world_state_item) = world_state_item {
            self.persist_rollout_items(&[RolloutItem::WorldState(world_state_item)])
                .await;
        }
        // A snapshot-only change does not require a duplicate TurnContext record.
        if only_world_state_changed {
            return Ok(world_state);
        }
        // Persist one `TurnContextItem` per real user turn so resume/lazy replay can recover the
        // latest durable baseline even when this turn emitted no model-visible context diffs.
        self.persist_rollout_items(&[RolloutItem::TurnContext(turn_context_item.clone())])
            .await;

        // Advance the persisted-settings baseline even when this turn emitted no model-visible
        // context items.
        let mut state = self.state.lock().await;
        state.set_reference_context_item(Some(turn_context_item));
        Ok(world_state)
    }

    pub(crate) async fn update_token_usage_info(
        &self,
        turn_context: &TurnContext,
        token_usage: Option<&TokenUsage>,
    ) -> CodexResult<()> {
        let result = self
            .record_token_usage_info(turn_context, token_usage)
            .await;
        self.send_token_count_event(turn_context).await;
        result
    }

    pub(crate) async fn record_observed_response_completed(
        &self,
        turn_context: &TurnContext,
        response_id: &str,
        usage: Option<&TokenUsage>,
        usage_metadata: Option<&ResponseUsageMetadata>,
    ) {
        self.send_event(
            turn_context,
            EventMsg::RawResponseCompleted(RawResponseCompletedEvent {
                response_id: response_id.to_string(),
                token_usage: usage.cloned(),
                usage_metadata: usage_metadata.cloned(),
            }),
        )
        .await;
        let Some(usage) = usage else {
            return;
        };
        let record = self.state.lock().await.record_token_usage(
            self.thread_id,
            &turn_context.sub_id,
            self.session_id(),
            turn_context
                .turn_metadata_state
                .root_turn_id()
                .unwrap_or_else(|| turn_context.sub_id.clone()),
            response_id.to_string(),
            usage,
        );
        self.persist_rollout_items(&[RolloutItem::TokenUsageRecord(record)])
            .await;
    }

    pub(crate) async fn record_token_usage_info(
        &self,
        turn_context: &TurnContext,
        token_usage: Option<&TokenUsage>,
    ) -> CodexResult<()> {
        if let Some(token_usage) = token_usage {
            let token_info = {
                let mut state = self.state.lock().await;
                state
                    .update_token_info_from_usage(token_usage, turn_context.model_context_window());
                if matches!(
                    turn_context.config.model_auto_compact_token_limit_scope,
                    AutoCompactTokenLimitScope::BodyAfterPrefix
                ) {
                    state.ensure_auto_compact_window_server_prefill_from_usage(token_usage);
                }
                state.token_info()
            };
            let budget_result = self.record_rollout_budget_usage(token_usage);
            if let Some(token_info) = token_info.as_ref() {
                for contributor in self.services.extensions.token_usage_contributors() {
                    contributor
                        .on_token_usage(
                            &self.services.session_extension_data,
                            &self.services.thread_extension_data,
                            turn_context.extension_data.as_ref(),
                            token_info,
                        )
                        .await;
                }
            }
            budget_result?;
        }
        Ok(())
    }

    pub(crate) async fn recompute_token_usage(&self, turn_context: &TurnContext) {
        let (history, server_reasoning_included) = {
            let state = self.state.lock().await;
            (state.clone_history(), state.server_reasoning_included())
        };
        let base_instructions = self.get_base_instructions().await;
        let Some(estimated_total_tokens) =
            history.estimate_token_count_with_base_instructions(&base_instructions)
        else {
            return;
        };
        // Active-context accounting adds items after the latest model-generated item separately
        // because no server response has reported them yet. Keep the reconstructed estimate in
        // the same shape so an incomplete resume/fork suffix is not counted twice.
        let estimated_local_tail_tokens =
            history.estimated_tokens_after_last_model_generated_item();
        let estimated_reasoning_supplement = if server_reasoning_included {
            0
        } else {
            history.estimated_non_last_reasoning_items_tokens()
        };
        let estimated_last_model_tokens = estimated_total_tokens
            .saturating_sub(estimated_local_tail_tokens)
            .saturating_sub(estimated_reasoning_supplement)
            .max(0);
        {
            let mut state = self.state.lock().await;
            let mut info = state.token_info().unwrap_or(TokenUsageInfo {
                total_token_usage: TokenUsage::default(),
                last_token_usage: TokenUsage::default(),
                model_context_window: None,
            });

            info.last_token_usage = TokenUsage {
                input_tokens: 0,
                cached_input_tokens: 0,
                cache_write_input_tokens: 0,
                output_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: estimated_last_model_tokens,
                codex_rollout_budget_units: None,
            };

            if let Some(model_context_window) = turn_context.model_context_window() {
                info.model_context_window = Some(model_context_window);
            }

            state.set_token_info(Some(info));
        }
        self.set_auto_compact_window_estimated_prefill_for_scope(
            turn_context,
            estimated_total_tokens,
        )
        .await;
        self.send_token_count_event(turn_context).await;
    }

    pub(crate) async fn update_rate_limits(
        &self,
        turn_context: &TurnContext,
        new_rate_limits: RateLimitSnapshot,
    ) {
        self.record_rate_limits_info(new_rate_limits).await;
        self.send_token_count_event(turn_context).await;
    }

    pub(crate) async fn record_rate_limits_info(&self, new_rate_limits: RateLimitSnapshot) {
        {
            let mut state = self.state.lock().await;
            state.set_rate_limits(new_rate_limits);
        }
    }

    pub(crate) async fn mcp_dependency_prompted(&self) -> HashSet<String> {
        let state = self.state.lock().await;
        state.mcp_dependency_prompted()
    }

    pub(crate) async fn record_mcp_dependency_prompted<I>(&self, names: I)
    where
        I: IntoIterator<Item = String>,
    {
        let mut state = self.state.lock().await;
        state.record_mcp_dependency_prompted(names);
    }

    pub(crate) async fn set_server_reasoning_included(&self, included: bool) {
        let mut state = self.state.lock().await;
        state.set_server_reasoning_included(included);
    }

    pub(crate) async fn send_token_count_event(&self, turn_context: &TurnContext) {
        let (info, rate_limits) = {
            let state = self.state.lock().await;
            state.token_info_and_rate_limits()
        };
        let event = EventMsg::TokenCount(TokenCountEvent { info, rate_limits });
        self.send_event(turn_context, event).await;
    }

    pub(crate) async fn set_total_tokens_full(&self, turn_context: &TurnContext) {
        if let Some(context_window) = turn_context.model_context_window() {
            let mut state = self.state.lock().await;
            state.set_token_usage_full(context_window);
        }
        self.send_token_count_event(turn_context).await;
    }

    pub(crate) async fn record_response_item_and_emit_turn_item(
        &self,
        turn_context: &TurnContext,
        response_item: ResponseItem,
    ) {
        // Add to conversation history and persist response item to rollout.
        self.record_conversation_items(turn_context, std::slice::from_ref(&response_item))
            .await;

        // Derive a turn item and emit lifecycle events if applicable.
        if let Some(item) = parse_turn_item(&response_item) {
            self.emit_turn_item_started(turn_context, &item).await;
            self.emit_turn_item_completed(turn_context, item).await;
        }
    }

    pub(crate) async fn record_prompt_and_emit_turn_item(
        &self,
        turn_context: &TurnContext,
        input: &[UserInput],
        client_id: Option<String>,
        persist_context: PersistContext,
        prompt_kind: PromptInputKind,
    ) {
        // Persist prompt content as user-role model input. User-authored prompts keep their
        // attachment spans, while attributed agent input emits a separate trusted presentation
        // instead of asking clients to infer provenance from model-visible marker text.
        let response_item = self.response_item_from_user_input(input.to_vec());
        self.record_conversation_items(turn_context, std::slice::from_ref(&response_item))
            .await;
        let turn_item = match prompt_kind {
            PromptInputKind::User => {
                let mut user_message_item = UserMessageItem::new(input);
                user_message_item.client_id = client_id;
                TurnItem::UserMessage(user_message_item)
            }
            PromptInputKind::Agent {
                presentation: AgentInputPresentation::Attributed(transcript),
            } => {
                let mut agent_message_item =
                    AgentMessageItem::new(&[AgentMessageContent::Text { text: transcript }]);
                agent_message_item.id = new_attributed_agent_message_response_item_id().to_string();
                agent_message_item.phase = Some(MessagePhase::Commentary);
                TurnItem::AgentMessage(agent_message_item)
            }
            PromptInputKind::Agent {
                presentation: AgentInputPresentation::Delegated(visible_input),
            } => {
                if visible_input.is_empty() {
                    self.ensure_rollout_materialized(persist_context).await;
                    return;
                }
                TurnItem::UserMessage(UserMessageItem::new(&visible_input))
            }
        };
        self.emit_turn_item_started(turn_context, &turn_item).await;
        self.emit_turn_item_completed(turn_context, turn_item).await;
        self.ensure_rollout_materialized(persist_context).await;
    }

    pub(crate) async fn notify_stream_error(
        &self,
        turn_context: &TurnContext,
        message: impl Into<String>,
        codex_error: CodexErr,
    ) {
        let additional_details = codex_error.to_string();
        let codex_error_info = CodexErrorInfo::ResponseStreamDisconnected {
            http_status_code: codex_error.http_status_code_value(),
        };
        let event = EventMsg::StreamError(StreamErrorEvent {
            message: message.into(),
            codex_error_info: Some(codex_error_info),
            additional_details: Some(additional_details),
        });
        self.send_event(turn_context, event).await;
    }

    pub(crate) async fn steer_internal_agent_input(
        &self,
        input: Vec<UserInput>,
        presentation: AgentInputPresentation,
        expected_turn_id: &str,
    ) -> Result<String, SteerInputError> {
        self.steer_input_with_response_observation_boundary_and_policy(
            PromptTurnDraft {
                input,
                additional_context: Default::default(),
                client_user_message_id: None,
                responsesapi_client_metadata: None,
                prompt_kind: PromptInputKind::Agent { presentation },
            },
            Some(expected_turn_id),
            InputTurnAdmissionPolicy::AnyTurn,
        )
        .await
        .map(|resolution| resolution.target_turn_id)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn steer_input_with_response_observation_boundary_and_policy(
        &self,
        draft: PromptTurnDraft,
        expected_turn_id: Option<&str>,
        policy: InputTurnAdmissionPolicy,
    ) -> Result<InputTurnAdmissionResolution, SteerInputError> {
        let PromptTurnDraft {
            input,
            additional_context,
            client_user_message_id,
            responsesapi_client_metadata,
            prompt_kind,
        } = draft;
        let mut active = self.active_turn.lock().await;
        let Some(active_turn) = active.as_mut() else {
            return Err(SteerInputError::NoActiveTurn(input));
        };

        let Some(active_task) = active_turn.task.as_ref() else {
            return Err(SteerInputError::NoActiveTurn(input));
        };
        let active_turn_id = &active_task.turn_context.sub_id;

        if policy == InputTurnAdmissionPolicy::Queued {
            return Err(SteerInputError::ActiveTurnPresent {
                actual: active_turn_id.clone(),
            });
        }

        if let Some(expected_turn_id) = expected_turn_id
            && expected_turn_id != active_turn_id
        {
            return Err(SteerInputError::ExpectedTurnMismatch {
                expected: expected_turn_id.to_string(),
                actual: active_turn_id.clone(),
            });
        }

        match active_task.kind {
            crate::state::TaskKind::Regular => {}
            crate::state::TaskKind::Review => {
                return Err(SteerInputError::ActiveTurnNotSteerable {
                    turn_kind: NonSteerableTurnKind::Review,
                });
            }
            crate::state::TaskKind::Compact => {
                return Err(SteerInputError::ActiveTurnNotSteerable {
                    turn_kind: NonSteerableTurnKind::Compact,
                });
            }
        }

        if input.is_empty() {
            return Err(SteerInputError::EmptyInput);
        }

        let additional_context_input = {
            let mut state = self.state.lock().await;
            state.additional_context.merge(additional_context)
        };

        if let Some(responsesapi_client_metadata) = responsesapi_client_metadata {
            active_task
                .turn_context
                .turn_metadata_state
                .set_responsesapi_client_metadata(responsesapi_client_metadata);
        }

        let mut pending_input = additional_context_input
            .into_iter()
            .map(ResponseItem::from)
            .map(|item| TurnInput::ResponseItem(item.into()))
            .collect::<Vec<_>>();
        pending_input.push(prompt_kind.into_turn_input(input, client_user_message_id.clone()));
        self.input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                active_turn.turn_state.as_ref(),
                pending_input,
            )
            .await;
        // `active` remains locked until this method returns, so the target cannot drain the newly
        // queued input before this exact response-event boundary is captured.
        Ok(self.capture_input_turn_admission_resolution(active_turn_id.clone()))
    }

    pub(crate) async fn record_memory_citation_for_turn(&self, sub_id: &str) {
        let turn_state = self
            .input_queue
            .turn_state_for_sub_id(&self.active_turn, sub_id)
            .await;
        let Some(turn_state) = turn_state else {
            return;
        };
        turn_state.lock().await.has_memory_citation = true;
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn get_pending_input(&self) -> Vec<TurnInput> {
        let (pending_input, accepts_mailbox_delivery, deferred_mcp_boundary) = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    let pending_input = ts.take_pending_input();
                    let mut current_turn_pending_input = Vec::new();
                    let mut completion_pending_input = Vec::new();
                    let mut defer_to_completion = false;
                    for item in pending_input {
                        if defer_to_completion || is_mcp_server_use_context_input_item(&item) {
                            defer_to_completion = true;
                            completion_pending_input.push(item);
                        } else {
                            current_turn_pending_input.push(item);
                        }
                    }
                    if !completion_pending_input.is_empty() {
                        ts.prepend_pending_input(completion_pending_input);
                    }
                    (
                        current_turn_pending_input,
                        ts.accepts_mailbox_delivery_for_current_turn(),
                        defer_to_completion,
                    )
                }
                None => (Vec::new(), true, false),
            }
        };
        if !accepts_mailbox_delivery || deferred_mcp_boundary {
            return pending_input;
        }
        let (mailbox_items, _) = self.input_queue.drain_mailbox_input_items().await;
        if pending_input.is_empty() {
            mailbox_items
        } else if mailbox_items.is_empty() {
            pending_input
        } else {
            let mut pending_input = pending_input;
            pending_input.extend(mailbox_items);
            pending_input
        }
    }

    pub async fn interrupt_task(self: &Arc<Self>) {
        info!("interrupt received: abort current task, if any");
        let had_active_turn = self.active_turn.lock().await.is_some();
        self.abort_all_tasks(TurnAbortReason::Interrupted).await;
        if !had_active_turn {
            self.cancel_mcp_startup();
        }
    }

    pub(crate) fn hooks(&self) -> Arc<Hooks> {
        self.services.hooks.load_full()
    }

    pub(crate) fn user_shell(&self) -> Arc<shell::Shell> {
        Arc::clone(&self.services.user_shell)
    }

    pub(crate) async fn current_rollout_path(&self) -> anyhow::Result<Option<PathBuf>> {
        let Some(live_thread) = self.live_thread() else {
            return Ok(None);
        };
        live_thread.local_rollout_path().await.map_err(Into::into)
    }

    pub(crate) async fn hook_transcript_path(&self) -> Option<PathBuf> {
        let rollout_path = match self.current_rollout_path().await {
            Ok(Some(path)) => path,
            Ok(None) => return None,
            Err(err) => {
                warn!("{err}");
                return None;
            }
        };
        self.ensure_rollout_materialized(PersistContext::Standard)
            .await;
        Some(rollout_path)
    }

    pub(crate) async fn take_pending_session_start_source(
        &self,
    ) -> Option<codex_hooks::SessionStartSource> {
        let mut state = self.state.lock().await;
        state.take_pending_session_start_source()
    }

    fn show_raw_agent_reasoning(&self) -> bool {
        self.services.show_raw_agent_reasoning
    }
}

pub(crate) fn emit_subagent_session_started(
    analytics_events_client: &AnalyticsEventsClient,
    client_metadata: AppServerClientMetadata,
    session_id: SessionId,
    thread_id: ThreadId,
    parent_thread_id: Option<ThreadId>,
    thread_config: ThreadConfigSnapshot,
    subagent_source: SubAgentSource,
) {
    let AppServerClientMetadata {
        client_name,
        client_version,
    } = client_metadata;
    if (client_name.is_none() || client_version.is_none())
        && subagent_source.kind() != crate::guardian::GUARDIAN_REVIEWER_NAME
    {
        tracing::warn!("skipping subagent thread analytics: missing inherited client metadata");
        return;
    }
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    analytics_events_client.track_subagent_thread_started(SubAgentThreadStartedInput {
        session_id: session_id.to_string(),
        thread_id: thread_id.to_string(),
        parent_thread_id: parent_thread_id.map(|thread_id| thread_id.to_string()),
        forked_from_thread_id: thread_config
            .forked_from_thread_id
            .map(|thread_id| thread_id.to_string()),
        product_client_id: thread_config.originator.clone(),
        client_name,
        client_version,
        model: thread_config.model,
        ephemeral: thread_config.ephemeral,
        thread_source: thread_config.thread_source,
        subagent_source,
        created_at,
    });
}

/// Builds hook configuration for one config snapshot, including any enabled plugin hooks.
async fn build_hooks_config(
    config: &Config,
    plugins_manager: &PluginsManager,
    environment: Option<&TurnEnvironment>,
) -> HooksConfig {
    let (hook_shell_program, hook_shell_argv) = environment
        .and_then(|environment| environment.shell.as_ref())
        .map(|shell| {
            let mut argv = shell.derive_exec_args("", /*use_login_shell*/ false);
            let program = argv.remove(0);
            let _ = argv.pop();
            (Some(program), argv)
        })
        .unwrap_or_default();
    let plugins_input = config.plugins_config_input();
    let plugin_outcome = plugins_manager.plugins_for_config(&plugins_input).await;
    let plugin_hook_sources = plugin_outcome.effective_plugin_hook_sources();
    let plugin_hook_load_warnings = plugin_outcome.effective_plugin_hook_warnings();
    HooksConfig {
        legacy_notify_argv: config.notify.clone(),
        feature_enabled: config.features.enabled(Feature::CodexHooks),
        bypass_hook_trust: config.bypass_hook_trust,
        config_layer_stack: Some(config.config_layer_stack.clone()),
        plugin_hook_sources,
        plugin_hook_load_warnings,
        shell_program: hook_shell_program,
        shell_args: hook_shell_argv,
    }
}

#[cfg(test)]
#[path = "elicitation_holders_tests.rs"]
mod elicitation_holders_tests;

#[cfg(test)]
#[path = "durable_context_tests.rs"]
mod durable_context_tests;

#[cfg(test)]
pub(crate) mod tests;
