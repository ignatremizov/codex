//! Explicit user observation and task context publication.

use super::response_observer::ResponseObserverStart;
use super::*;
use crate::CodexThread;
use crate::context::AgentContextIdentity;
use crate::context::ContextualUserFragment;
use crate::context::UserAgentTask;
use codex_protocol::protocol::new_user_agent_task_context_response_item_id;

pub(crate) struct ReplacedFinalResponseObservation {
    pub(crate) target_thread_id: ThreadId,
    pub(crate) previous: FinalResponseObservation,
    pub(crate) binding: ReplacedFinalResponseObservationBinding,
}

impl LocalAgentControl {
    pub(crate) async fn ensure_durable_completion_watcher(
        &self,
        target_id: ThreadId,
        source: SessionSource,
        policy: ResponseObservationPolicy,
        observed_status: AgentStatus,
        task_preview: Option<String>,
    ) -> CodexResult<AgentStatus> {
        let control = self.clone();
        tokio::spawn(async move {
            let state = control.upgrade()?;
            let _lifecycle = state.acquire_live_agent_lifecycle(target_id).await?;
            control.require_current_agent_ownership(target_id).await?;
            let target = state.get_thread(target_id).await?;
            let parent_id = source.parent_thread_id().ok_or_else(|| {
                CodexErr::InvalidRequest("user observation requires a source thread".into())
            })?;
            let observer = state.get_thread(parent_id).await?;
            let _admission = observer
                .session
                .submission_admission
                .try_accept_completion_delivery()
                .ok_or_else(|| CodexErr::InvalidRequest("observer is closing".into()))?;
            observer.session.submission_admission.check_ready()?;
            let parent = observer.session.presentation_id();
            control.ensure_target_message_route_allowed(&target, parent, policy)?;
            let delivered_final_turns =
                super::resume_delivery::delivered_final_turns(&observer, target_id).await;
            let _permission = control.acquire_messaging_permission_transaction().await;
            let transaction = control
                .acquire_response_observation_transaction(parent)
                .await;
            control
                .install_response_observer(
                    &observer,
                    &target,
                    policy,
                    ResponseObservationBinding::NextTurn,
                    ResponseObserverStart::CurrentOrNext {
                        observed_status,
                        delivered_final_turns,
                    },
                )
                .await?;
            let child = target.session.presentation_id();
            let (snapshot, subscription) = target.session.subscribe_agent_responses();
            drop(subscription);
            if let Some((_, turn_id)) = control.user_observation_binding(
                parent,
                child,
                snapshot.active_turn_id.as_deref(),
                snapshot
                    .last_terminal
                    .as_ref()
                    .map(|(turn, _)| turn.as_str()),
            ) {
                if let Err(error) = control
                    .publish_user_task_observation(
                        &observer,
                        child,
                        turn_id,
                        policy,
                        task_preview,
                        transaction,
                    )
                    .await
                {
                    control.abandon_response_observer(parent, child, &error.to_string());
                    return Err(error);
                }
            }
            Ok(target.agent_status().await)
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("user observation worker failed: {error}")))?
    }

    pub(crate) async fn current_response_observation_binding_for_thread(
        &self,
        parent: SessionPresentationId,
        target_id: ThreadId,
    ) -> Option<ReplacedFinalResponseObservationBinding> {
        let state = self.upgrade().ok()?;
        let target = state.get_thread(target_id).await.ok()?;
        let (snapshot, subscription) = target.session.subscribe_agent_responses();
        drop(subscription);
        self.user_observation_binding(
            parent,
            target.session.presentation_id(),
            snapshot.active_turn_id.as_deref(),
            snapshot
                .last_terminal
                .as_ref()
                .map(|(turn, _)| turn.as_str()),
        )
        .map(|(binding, _)| binding)
    }

    pub(crate) async fn replace_durable_final_response_observation(
        &self,
        target_id: ThreadId,
        source: SessionPresentationId,
        observer_thread_id: ThreadId,
        replacement: FinalResponseObservation,
    ) -> CodexResult<ReplacedFinalResponseObservation> {
        let control = self.clone();
        tokio::spawn(async move {
            let state = control.upgrade()?;
            let _lifecycle = state.acquire_live_agent_lifecycle(target_id).await?;
            control.require_current_agent_ownership(target_id).await?;
            control
                .require_current_agent_ownership(observer_thread_id)
                .await?;
            control
                .require_current_agent_ownership(source.thread_id)
                .await?;
            if target_id == observer_thread_id {
                return Err(CodexErr::InvalidRequest(
                    "an agent cannot observe itself".into(),
                ));
            }
            let target = state.get_thread(target_id).await?;
            let actor = state.get_thread(source.thread_id).await?;
            let observer = state.get_thread(observer_thread_id).await?;
            if actor.session.presentation_id() != source
                || !Arc::ptr_eq(&control.state, &actor.session.services.agent_control.state)
            {
                return Err(CodexErr::ThreadNotFound(source.thread_id));
            }
            if !Arc::ptr_eq(
                &control.state,
                &observer.session.services.agent_control.state,
            ) {
                return Err(CodexErr::InvalidRequest(
                    "observer belongs to another control".into(),
                ));
            }
            actor.session.submission_admission.check_ready()?;
            // Retain the issuing actor independently of the selected observer through ACK.
            // A close/transfer cannot turn this into a control action by a replacement runtime.
            let actor_admission = actor
                .session
                .submission_admission
                .try_accept_completion_delivery()
                .ok_or_else(|| CodexErr::InvalidRequest("issuing thread is closing".into()))?;
            let parent = observer.session.presentation_id();
            let _admission = observer
                .session
                .submission_admission
                .try_accept_completion_delivery()
                .ok_or_else(|| CodexErr::InvalidRequest("observer is closing".into()))?;
            observer.session.submission_admission.check_ready()?;
            target.session.submission_admission.check_ready()?;
            let _permission = control.acquire_messaging_permission_transaction().await;
            let transaction = control
                .acquire_response_observation_transaction(parent)
                .await;
            let child = target.session.presentation_id();
            let (snapshot, subscription) = target.session.subscribe_agent_responses();
            drop(subscription);
            let (binding, turn_id) = control
                .user_observation_binding(
                    parent,
                    child,
                    snapshot.active_turn_id.as_deref(),
                    snapshot
                        .last_terminal
                        .as_ref()
                        .map(|(turn, _)| turn.as_str()),
                )
                .ok_or_else(|| {
                    CodexErr::InvalidRequest("no pending response observation to replace".into())
                })?;
            let (preview, promoted, previous) = control
                .user_observation_task(parent, child, turn_id.as_deref())
                .ok_or_else(|| {
                    CodexErr::InvalidRequest("response observation is no longer live".into())
                })?;
            let task = if !promoted
                && matches!(
                    replacement,
                    FinalResponseObservation::Passive | FinalResponseObservation::Wake
                ) {
                match preview {
                    Some(preview) => Some(
                        control
                            .user_task_item(&observer, target_id, preview)
                            .await?,
                    ),
                    None => None,
                }
            } else {
                None
            };
            let prepared = control.prepare_user_observation(
                parent,
                child,
                turn_id.clone(),
                Some(replacement),
                /*task_preview*/ None,
                task.as_ref(),
            )?;
            let mailbox_subscription_to_retire = control
                .active_mailbox_subscription_for_policy(&observer, child)
                .await?;
            let snapshots = prepared.snapshots.clone();
            let commit_control = control.clone();
            let result = observer
                .session
                .commit_user_agent_task(transaction, snapshots, task, move || {
                    commit_control.commit_user_observation(prepared)
                })
                .await;
            if let Err(error) = result {
                control.abandon_response_observer(parent, child, &error.to_string());
                return Err(error);
            }
            if let Some(message_id) = mailbox_subscription_to_retire {
                control
                    .retire_mailbox_subscription_token(&observer, child, &message_id)
                    .await?;
                let transaction = control
                    .acquire_response_observation_transaction(parent)
                    .await;
                control.clear_mailbox_final_subscription(
                    parent,
                    child,
                    &message_id,
                    turn_id.as_deref(),
                );
                control
                    .persist_response_observation_snapshot(parent, child)
                    .await?;
                drop(transaction);
            }
            drop(actor_admission);
            drop(_permission);
            control.recheck_thread_idle_lifecycle(parent).await;
            Ok(ReplacedFinalResponseObservation {
                target_thread_id: target_id,
                previous,
                binding,
            })
        })
        .await
        .map_err(|error| {
            CodexErr::Fatal(format!("observation replacement worker failed: {error}"))
        })?
    }

    pub(super) async fn publish_user_task_observation(
        &self,
        observer: &Arc<CodexThread>,
        child: SessionPresentationId,
        turn_id: Option<String>,
        policy: ResponseObservationPolicy,
        preview: Option<String>,
        transaction: tokio::sync::OwnedMutexGuard<()>,
    ) -> CodexResult<()> {
        let parent = observer.session.presentation_id();
        let preview = preview.and_then(non_empty_task_message);
        let task = if policy.exposes_source_model_context() {
            match preview.as_ref() {
                Some(preview) => Some(
                    self.user_task_item(observer, child.thread_id, preview.clone())
                        .await?,
                ),
                None => None,
            }
        } else {
            None
        };
        let prepared = self.prepare_user_observation(
            parent,
            child,
            turn_id,
            /*final_response*/ None,
            preview,
            task.as_ref(),
        )?;
        let snapshots = prepared.snapshots.clone();
        let control = self.clone();
        observer
            .session
            .commit_user_agent_task(transaction, snapshots, task, move || {
                control.commit_user_observation(prepared)
            })
            .await
    }

    async fn user_task_item(
        &self,
        observer: &Arc<CodexThread>,
        target_id: ThreadId,
        preview: String,
    ) -> CodexResult<ResponseItem> {
        let agent = self
            .model_visible_agent_identity_for_version(
                observer
                    .multi_agent_version()
                    .unwrap_or(MultiAgentVersion::V1),
                target_id,
            )
            .await?;
        let preview = preview.chars().take(240).collect::<String>();
        let task = UserAgentTask::new(agent, preview);
        Ok(ResponseItem::Message {
            id: Some(new_user_agent_task_context_response_item_id()),
            role: "user".into(),
            content: vec![ContentItem::InputText {
                text: task.render(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        })
    }

    pub(crate) async fn model_visible_agent_identity_for_version(
        &self,
        version: MultiAgentVersion,
        target_id: ThreadId,
    ) -> CodexResult<AgentContextIdentity> {
        let metadata = self.get_agent_metadata(target_id);
        let is_root = self
            .bound_session_id()
            .is_some_and(|id| ThreadId::from(id) == target_id);
        Ok(match version {
            MultiAgentVersion::V1 => {
                let alias = self.find_session_agent_alias(target_id).await?;
                AgentContextIdentity::V1 {
                    agent_id: target_id,
                    agent_ref: alias.as_ref().map(|alias| alias.agent_ref),
                    task_path: alias.as_ref().and_then(|alias| alias.task_path.clone()),
                    nickname: alias
                        .and_then(|alias| alias.nickname)
                        .or_else(|| {
                            metadata
                                .as_ref()
                                .and_then(|metadata| metadata.agent_nickname.clone())
                        })
                        .or_else(|| {
                            is_root.then(|| codex_protocol::MAIN_AGENT_NICKNAME.to_owned())
                        }),
                }
            }
            MultiAgentVersion::Disabled | MultiAgentVersion::V2 => match metadata
                .and_then(|metadata| metadata.agent_path)
                .or_else(|| is_root.then(AgentPath::root))
            {
                Some(agent_path) => AgentContextIdentity::V2 {
                    agent_id: target_id,
                    agent_path,
                },
                None => AgentContextIdentity::Canonical {
                    agent_id: target_id,
                },
            },
        })
    }
}
