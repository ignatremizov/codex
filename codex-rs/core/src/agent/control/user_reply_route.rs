//! User-authorized relationship routing: target context, source receipt, then live permission.

use super::scoped_messages::AgentReplyRouteLifetime;
use super::*;
use crate::CodexThread;

struct RouteMutation {
    control: LocalAgentControl,
    observer: Arc<CodexThread>,
    parent: SessionPresentationId,
    child: SessionPresentationId,
    committed: bool,
}

impl Drop for RouteMutation {
    fn drop(&mut self) {
        if !self.committed {
            let reason = "reply-route outcome unknown; reload required, do not retry";
            self.observer.session.quarantine_history(reason.to_string());
            self.control
                .abandon_response_observer(self.parent, self.child, reason);
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
        parent: SessionPresentationId,
        mode: TargetMessageRouteMode,
    ) -> CodexResult<ReplacedTargetMessageRoute> {
        let control = self.clone();
        tokio::spawn(async move {
            let state = control.upgrade()?;
            if parent.thread_id == target_thread_id {
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
            let _source = state.agent_turn_queue.acquire_source_admission(parent.thread_id).await;
            let observer = state.get_thread(parent.thread_id).await?;
            if observer.session.presentation_id() != parent {
                return Err(CodexErr::ThreadNotFound(parent.thread_id));
            }
            observer.session.submission_admission.check_ready()?;
            target.session.submission_admission.check_ready()?;
            let _observer_admission = observer.session.submission_admission
                .try_accept_completion_delivery()
                .ok_or_else(|| CodexErr::InvalidRequest("observer is closing".into()))?;
            let _target_admission = target.session.submission_admission
                .try_accept_completion_delivery()
                .ok_or_else(|| CodexErr::InvalidRequest("target is closing".into()))?;
            let transaction = control.acquire_response_observation_transaction(parent).await;
            let child = target.session.presentation_id();
            let prepared = control.prepare_reply_route(parent, child, mode)?;
            let previous = prepared.previous;
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
                control: control.clone(),
                observer: Arc::clone(&observer),
                parent,
                child,
                committed: false,
            };
            if let Some(route) = route {
                target.session.publish_persistent_reply_route(route).await.map_err(|error| {
                    CodexErr::Fatal(format!("reply-route outcome unknown; do not retry: {error}"))
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
                    "reply-route outcome unknown; do not retry: {error}"
                )));
            }
            mutation.committed = true;
            Ok(ReplacedTargetMessageRoute { target_thread_id, previous })
        }).await.map_err(|error| {
            CodexErr::Fatal(format!("reply-route replacement worker failed: {error}; do not retry"))
        })?
    }
}
