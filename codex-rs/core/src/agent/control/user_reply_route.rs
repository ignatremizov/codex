//! User-authorized relationship routing: target context, source receipt, then live permission.

use super::scoped_messages::AgentReplyRouteLifetime;
use super::*;
use crate::CodexThread;

struct RouteMutation {
    observer: Arc<CodexThread>,
    committed: bool,
}

impl Drop for RouteMutation {
    fn drop(&mut self) {
        if !self.committed {
            let reason = "reply-route outcome unknown; reload required, do not retry";
            self.observer.session.quarantine_history(reason.to_string());
        }
    }
}

pub(crate) struct ReplacedTargetMessageRoute {
    pub(crate) target_thread_id: ThreadId,
    pub(crate) previous: Option<TargetMessageRouteMode>,
}

impl LocalAgentControl {
    pub(crate) async fn replace_durable_target_message_route(
        &self,
        target_thread_id: ThreadId,
        recipient_thread_id: ThreadId,
        mode: TargetMessageRouteMode,
    ) -> CodexResult<ReplacedTargetMessageRoute> {
        let control = self.clone();
        tokio::spawn(async move {
            let state = control.upgrade()?;
            if recipient_thread_id == target_thread_id {
                return Err(CodexErr::InvalidRequest("an agent cannot grant itself a reply route".into()));
            }
            let _lifecycle = state.acquire_live_agent_lifecycle(target_thread_id).await?;
            control.require_current_agent_ownership(target_thread_id).await?;
            let target = state.get_thread(target_thread_id).await?;
            if target.multi_agent_version() == Some(MultiAgentVersion::V2) {
                return Err(CodexErr::UnsupportedOperation(
                    "V2 targets use native inter-agent messaging and do not support V1 reply routes".into(),
                ));
            }
            let _source = state.agent_turn_queue.acquire_source_admission(recipient_thread_id).await;
            control.require_current_agent_ownership(recipient_thread_id).await?;
            let observer = state.get_thread(recipient_thread_id).await?;
            let parent = observer.session.presentation_id();
            observer.session.submission_admission.check_ready()?;
            target.session.submission_admission.check_ready()?;
            let _observer_admission = observer.session.submission_admission
                .try_accept_completion_delivery()
                .ok_or_else(|| CodexErr::InvalidRequest("observer is closing".into()))?;
            let _target_admission = target.session.submission_admission
                .try_accept_completion_delivery()
                .ok_or_else(|| CodexErr::InvalidRequest("target is closing".into()))?;
            let _permission = control.acquire_messaging_permission_transaction().await;
            let transaction = control.acquire_response_observation_transaction(parent).await;
            let child = target.session.presentation_id();
            control.restore_agent_send_pair_locked(child, parent).await?;
            let previous = control.prepare_reply_route(parent, child, mode)?.previous;
            let route = if mode.is_enabled() {
                Some(control.agent_reply_route_item(
                    &target,
                    parent,
                    AgentReplyRouteLifetime::UntilDisabled,
                ).await?)
            } else {
                None
            };
            let mut mutation = RouteMutation {
                observer: Arc::clone(&observer),
                committed: false,
            };
            control.persist_agent_send_setting_locked(
                codex_agent_graph_store::AgentSendScope::Directed {
                    sender_thread_id: target_thread_id,
                    receiver_thread_id: recipient_thread_id,
                },
                mode,
            ).await?;
            // The SQL setting is authoritative independently of the following canonical audit.
            // Keep it effective even if either endpoint loses its publication receipt.
            control.install_persisted_send_mode(parent, child, mode);
            let prepared = control.prepare_reply_route(parent, child, mode)?;
            if let Some(route) = route {
                target.session.publish_persistent_reply_route(route).await.map_err(|error| {
                    CodexErr::Fatal(format!("messaging setting committed but target context publication failed: {error}; reload before continuing"))
                })?;
            }
            let snapshots = prepared.snapshots.clone();
            let commit_control = control.clone();
            if let Err(error) = observer.session.commit_user_agent_task(
                transaction,
                snapshots,
                /*task*/ None,
                move || commit_control.commit_reply_route(prepared),
            ).await {
                return Err(CodexErr::Fatal(format!(
                    "messaging setting committed but observation audit failed: {error}; reload before continuing"
                )));
            }
            mutation.committed = true;
            control.refresh_messaging_context_locked(target_thread_id).await.map_err(|error| {
                CodexErr::Fatal(format!("send permission committed but context refresh failed: {error}; do not retry"))
            })?;
            Ok(ReplacedTargetMessageRoute { target_thread_id, previous })
        }).await.map_err(|error| {
            CodexErr::Fatal(format!("reply-route replacement worker failed: {error}; do not retry"))
        })?
    }
}
