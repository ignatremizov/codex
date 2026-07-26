//! Human command registration and presentation metadata; policy routing stays with tools.

use std::sync::Arc;
use std::sync::PoisonError;

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

use super::NetworkApprovalTurn;
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
        timing_call_id: Option<&str>,
        approval_id: Option<String>,
        environment_id: Option<String>,
        command: Vec<String>,
        cwd: PathUri,
        reason: Option<String>,
        network_approval: Option<(NetworkApprovalContext, NetworkApprovalTurn)>,
        proposed_execpolicy_amendment: Option<ExecPolicyAmendment>,
        additional_permissions: Option<AdditionalPermissionProfile>,
        available_decisions: Option<Vec<ReviewDecision>>,
        plugin_attribution_override: Option<PluginCommandAttribution>,
    ) -> ReviewDecision {
        let timeout_ms = turn_context.config.approval_timeout_ms;
        if timeout_ms == Some(0) {
            return ReviewDecision::TimedOut;
        }
        let (network_approval_context, turn_binding) = network_approval.unzip();
        let user_wait_owner = self.user_wait_timing_owner(turn_context, timing_call_id);
        let started_at_ms = now_unix_timestamp_ms();
        let expires_at_ms = timeout_ms.map(|timeout_ms| {
            started_at_ms.saturating_add(i64::try_from(timeout_ms).unwrap_or(i64::MAX))
        });
        let deadline = timeout_ms
            .map(|timeout_ms| saturating_instant_add_ms(tokio::time::Instant::now(), timeout_ms));
        let origin = if kind == ExecApprovalKind::HumanCommand {
            CommandApprovalOrigin::HumanCommand
        } else if network_approval_context.is_some() {
            CommandApprovalOrigin::Network
        } else {
            match kind {
                ExecApprovalKind::WriteStdin => CommandApprovalOrigin::WriteStdin,
                ExecApprovalKind::HumanCommand => CommandApprovalOrigin::HumanCommand,
                ExecApprovalKind::Command if approval_id.is_some() => {
                    CommandApprovalOrigin::NestedExecve
                }
                ExecApprovalKind::Command => CommandApprovalOrigin::RootCommand,
            }
        };
        let approval_id = Uuid::new_v4().to_string();
        let (originating_turn, waiter, code_mode_binding) = {
            let active = self.active_turn.lock().await;
            let Some(active) = active.as_ref() else {
                return ReviewDecision::Abort;
            };
            let Some(task) = active.task.as_ref() else {
                return ReviewDecision::Abort;
            };
            let code_mode_binding = if turn_binding != Some(NetworkApprovalTurn::AnyActive)
                && task.turn_context.sub_id != turn_context.sub_id
            {
                self.code_mode_command_approval_binding(
                    active,
                    &turn_context.sub_id,
                    timing_call_id.unwrap_or(&call_id),
                )
            } else {
                None
            };
            if task.cancellation_token.is_cancelled()
                || (turn_binding != Some(NetworkApprovalTurn::AnyActive)
                    && task.turn_context.sub_id != turn_context.sub_id
                    && code_mode_binding.is_none())
            {
                return ReviewDecision::Abort;
            }
            // Yielded requests retain their original presentation turn. Code Mode also
            // requires the live call lease captured from this exact servicing task.
            let originating_turn = Arc::clone(&active.turn_state);
            let (pending, mut waiter) = PendingCommandApproval::new(
                &originating_turn,
                turn_context.sub_id.clone(),
                task.turn_context.sub_id.clone(),
                kind,
                origin,
                deadline,
            );
            if let Some(binding) = &code_mode_binding {
                pending.bind_code_mode(binding);
            }
            {
                let state = originating_turn.lock().await;
                if network_approval_context.is_some() {
                    waiter.track_network_prompt(state.track_network_prompt(
                        &self.tx_event,
                        &turn_context.sub_id,
                        &approval_id,
                        pending.identity(),
                    ));
                }
                state
                    .command_approvals
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(approval_id.clone(), pending);
            }
            // The waiter must not keep its own pending registration alive after
            // the servicing turn ends.
            (Arc::downgrade(&originating_turn), waiter, code_mode_binding)
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
        let _user_wait = self.start_user_wait_timing(turn_context, user_wait_owner.as_deref());
        self.send_event(turn_context, event).await;
        let decision = if let Some(binding) = code_mode_binding {
            tokio::select! {
                biased;
                () = binding.cancelled() => ReviewDecision::Abort,
                decision = waiter.wait() => decision,
            }
        } else {
            waiter.wait().await
        };
        if let Some(originating_turn) = originating_turn.upgrade() {
            originating_turn
                .lock()
                .await
                .command_approvals
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&approval_id);
        }
        decision
    }
}
