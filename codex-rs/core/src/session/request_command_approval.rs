//! Human command registration and presentation metadata; policy routing stays with tools.

use std::sync::Arc;

use codex_core_plugins::PluginCommandAttribution;
use codex_protocol::approvals::ExecApprovalKind;
use codex_protocol::approvals::ExecApprovalRequestEvent;
use codex_protocol::approvals::ExecPolicyAmendment;
use codex_protocol::approvals::NetworkPolicyAmendment;
use codex_protocol::approvals::NetworkPolicyRuleAction;
use codex_protocol::items::ModelInvocationContext;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::NetworkApprovalContext;
use codex_protocol::protocol::ReviewDecision;
use codex_shell_command::parse_command::parse_command;
use codex_utils_path_uri::PathUri;
use uuid::Uuid;

use super::Session;
use super::command_approval::CommandApprovalOrigin;
use super::command_approval::PendingCommandApproval;
use super::command_approval::saturating_instant_add_ms;
use super::now_unix_timestamp_ms;
use super::turn_context::TurnContext;

impl Session {
    /// Emit an exec approval request event and await the user's decision.
    ///
    /// Fresh opaque callback IDs identify one human request, independently of its command item.
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
        model_context: ModelInvocationContext,
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
        let timeout_ms = turn_context.config.approval_timeout_ms;
        if timeout_ms == Some(0) {
            return ReviewDecision::TimedOut;
        }
        let started_at_ms = now_unix_timestamp_ms();
        let expires_at_ms = timeout_ms.map(|timeout_ms| {
            started_at_ms.saturating_add(i64::try_from(timeout_ms).unwrap_or(i64::MAX))
        });
        let deadline = timeout_ms
            .map(|timeout_ms| saturating_instant_add_ms(tokio::time::Instant::now(), timeout_ms));
        let origin = if network_approval_context.is_some() {
            CommandApprovalOrigin::Network
        } else {
            match kind {
                ExecApprovalKind::WriteStdin => CommandApprovalOrigin::WriteStdin,
                ExecApprovalKind::Command if approval_id.is_some() => {
                    CommandApprovalOrigin::NestedExecve
                }
                ExecApprovalKind::Command => CommandApprovalOrigin::RootCommand,
            }
        };
        let approval_id = Uuid::new_v4().to_string();
        let (originating_turn, waiter) = {
            let active = self.active_turn.lock().await;
            let Some(active) = active.as_ref() else {
                return ReviewDecision::Abort;
            };
            if !active.task.as_ref().is_some_and(|task| {
                task.turn_context.sub_id == turn_context.sub_id
                    && !task.cancellation_token.is_cancelled()
            }) {
                return ReviewDecision::Abort;
            }
            let originating_turn = Arc::clone(&active.turn_state);
            let (pending, waiter) = PendingCommandApproval::new(
                &originating_turn,
                turn_context.sub_id.clone(),
                origin,
                deadline,
            );
            originating_turn
                .lock()
                .await
                .command_approvals
                .insert(approval_id.clone(), pending);
            (originating_turn, waiter)
        };
        let _elicitation = self.services.elicitations.register();

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
            model_context: Some(model_context),
            kind,
            call_id,
            plugin_id,
            script_path,
            approval_id: Some(approval_id.clone()),
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
        let decision = waiter.wait().await;
        originating_turn
            .lock()
            .await
            .command_approvals
            .remove(&approval_id);
        decision
    }
}
