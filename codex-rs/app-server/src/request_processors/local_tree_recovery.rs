//! Local replacement-server recovery is deliberately split into preparation and admission.
//! Preparation attaches the complete available tree without waking idle lifecycle work.
//! Only durable, unfinished, receiver-owned regular turns are eligible for admission.

use super::ThreadRequestProcessor;
use super::ThreadResumeTarget;
use crate::outgoing_message::ConnectionId;
use codex_app_server_protocol::ThreadEnvironment;
use codex_app_server_protocol::ThreadResumeParams;
use codex_core::StartIfIdleSubmission;
use codex_core::ThreadManager;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::turn_input::RecoverTurnRequest;
use codex_rollout::RolloutItem;
use codex_state::AgentAliasState;
use codex_state::DirectionalThreadSpawnEdgeStatus;
use codex_thread_store::ListTurnsParams;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::SortDirection;
use codex_thread_store::StoredTurnItemsView;
use codex_thread_store::StoredTurnStatus;
use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::sync::Weak;

#[path = "local_tree_recovery_calls.rs"]
mod calls;
#[path = "local_tree_recovery_settings.rs"]
mod settings;
use calls::validate_call_outcomes;
use settings::validate_recovery_settings;

/// A single-use recovery plan bound to the replacement server that prepared it.
///
/// Dropping this plan leaves restored threads idle. It does not submit continuation input.
pub struct PreparedLocalTreeRecovery {
    manager: Weak<ThreadManager>,
    root_thread_id: ThreadId,
    thread_ids: Vec<ThreadId>,
    warnings: Vec<String>,
    candidates: Vec<(ThreadId, RecoveryCandidate)>,
}

impl PreparedLocalTreeRecovery {
    pub fn root_thread_id(&self) -> ThreadId {
        self.root_thread_id
    }

    pub fn thread_ids(&self) -> &[ThreadId] {
        &self.thread_ids
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// Per-thread admission results; a paused child does not invalidate the usable session.
#[derive(Debug, Default)]
pub struct LocalTreeRecoveryOutcome {
    pub resumed_turns: Vec<ThreadId>,
    pub paused_threads: Vec<(ThreadId, String)>,
}

struct RecoveryCandidate {
    context: TurnContextItem,
    fingerprint: serde_json::Value,
}

impl ThreadRequestProcessor {
    pub(crate) async fn prepare_local_tree_recovery(
        &self,
        selected: ThreadId,
        connection_id: ConnectionId,
    ) -> io::Result<PreparedLocalTreeRecovery> {
        let db = self.state_db.as_ref().ok_or_else(|| {
            io::Error::other("agent-tree recovery requires persisted ownership state")
        })?;
        let mut root = selected;
        let mut ancestors = HashSet::from([selected]);
        while let Some(parent) = db
            .find_thread_spawn_parent(root)
            .await
            .map_err(io::Error::other)?
        {
            let open = db
                .list_thread_spawn_children_with_status(
                    parent,
                    DirectionalThreadSpawnEdgeStatus::Open,
                )
                .await
                .map_err(io::Error::other)?;
            if !open.contains(&root) {
                return Err(io::Error::other(
                    "selected thread has a closed ownership edge",
                ));
            }
            if !ancestors.insert(parent) || ancestors.len() > 1024 {
                return Err(io::Error::other(
                    "invalid or oversized agent ownership chain",
                ));
            }
            root = parent;
        }
        let descendants = db
            .list_thread_spawn_descendants_with_status(root, DirectionalThreadSpawnEdgeStatus::Open)
            .await
            .map_err(io::Error::other)?;
        if descendants.len() > 1024 {
            return Err(io::Error::other("agent tree exceeds local recovery limit"));
        }
        let mut plan = PreparedLocalTreeRecovery {
            manager: Arc::downgrade(&self.thread_manager),
            root_thread_id: root,
            thread_ids: Vec::new(),
            warnings: Vec::new(),
            candidates: Vec::new(),
        };
        let mut prepared = HashSet::new();
        for thread_id in std::iter::once(root).chain(descendants) {
            let result = async {
                if let Some(alias) = db
                    .find_current_agent_alias_by_thread(thread_id)
                    .await
                    .map_err(io::Error::other)?
                    && (alias.state != AgentAliasState::Active || alias.session_id != root.into())
                {
                    return Err(io::Error::other(
                        "thread belongs to another or closed agent tree",
                    ));
                }
                if thread_id != root {
                    let parent = db
                        .find_thread_spawn_parent(thread_id)
                        .await
                        .map_err(io::Error::other)?;
                    if !parent.is_some_and(|parent| prepared.contains(&parent)) {
                        return Err(io::Error::other("owning parent could not be restored"));
                    }
                }
                self.thread_resume(
                    ThreadResumeTarget::LocalTreeRecovery(connection_id),
                    ThreadResumeParams {
                        thread_id: thread_id.to_string(),
                        exclude_turns: true,
                        ..Default::default()
                    },
                    /*app_server_client_name*/ None,
                    /*app_server_client_version*/ None,
                    Default::default(),
                )
                .await
                .map_err(|err| io::Error::other(err.message))?;
                Ok::<(), io::Error>(())
            }
            .await;
            if let Err(err) = result {
                if ancestors.contains(&thread_id) {
                    return Err(io::Error::other(format!(
                        "cannot restore required thread {thread_id}: {err}"
                    )));
                }
                plan.warnings
                    .push(format!("Agent {thread_id} was not restored: {err}"));
                continue;
            }
            prepared.insert(thread_id);
            plan.thread_ids.push(thread_id);
            match self.local_recovery_candidate(thread_id).await {
                Ok(Some(candidate)) => plan.candidates.push((thread_id, candidate)),
                Ok(None) => {}
                Err(err) => plan
                    .warnings
                    .push(format!("Agent {thread_id} remains paused: {err}")),
            }
        }
        if !prepared.contains(&selected) {
            return Err(io::Error::other(
                "selected thread is not in the active ownership tree",
            ));
        }
        Ok(plan)
    }

    pub(crate) async fn continue_local_tree_recovery(
        &self,
        plan: PreparedLocalTreeRecovery,
    ) -> io::Result<LocalTreeRecoveryOutcome> {
        if !Weak::ptr_eq(&plan.manager, &Arc::downgrade(&self.thread_manager)) {
            return Err(io::Error::other("recovery plan belongs to another server"));
        }
        let mut outcome = LocalTreeRecoveryOutcome::default();
        for (thread_id, expected) in plan.candidates {
            let result = async {
                let db = self
                    .state_db
                    .as_ref()
                    .ok_or_else(|| io::Error::other("ownership state unavailable"))?;
                if let Some(alias) = db
                    .find_current_agent_alias_by_thread(thread_id)
                    .await
                    .map_err(io::Error::other)?
                    && (alias.state != AgentAliasState::Active
                        || alias.session_id != plan.root_thread_id.into())
                {
                    return Err(io::Error::other(
                        "agent ownership changed after preparation",
                    ));
                }
                if thread_id != plan.root_thread_id
                    && !db
                        .list_thread_spawn_descendants_with_status(
                            plan.root_thread_id,
                            DirectionalThreadSpawnEdgeStatus::Open,
                        )
                        .await
                        .map_err(io::Error::other)?
                        .contains(&thread_id)
                {
                    return Err(io::Error::other("agent is no longer an open descendant"));
                }
                let thread = self
                    .thread_manager
                    .get_thread(thread_id)
                    .await
                    .map_err(io::Error::other)?;
                let current = self
                    .local_recovery_candidate(thread_id)
                    .await?
                    .ok_or_else(|| io::Error::other("turn completed or no longer eligible"))?;
                if current.fingerprint != expected.fingerprint {
                    return Err(io::Error::other(
                        "durable work or runtime settings changed after preparation",
                    ));
                }
                let turn_id = current
                    .context
                    .turn_id
                    .ok_or_else(|| io::Error::other("missing turn identity"))?;
                match thread
                    .recover_turn_if_still_current(RecoverTurnRequest {
                        turn_id,
                        thread_settings: Default::default(),
                        trace: None,
                        cyber_access_program: current.context.cyber_access_program,
                    })
                    .await
                    .map_err(io::Error::other)?
                {
                    StartIfIdleSubmission::Started { .. } => Ok(()),
                    StartIfIdleSubmission::NotSubmitted { reason } => {
                        Err(io::Error::other(format!("turn not admitted: {reason:?}")))
                    }
                }
            }
            .await;
            match result {
                Ok(()) => outcome.resumed_turns.push(thread_id),
                Err(err) => outcome.paused_threads.push((thread_id, err.to_string())),
            }
        }
        Ok(outcome)
    }

    async fn local_recovery_candidate(
        &self,
        thread_id: ThreadId,
    ) -> io::Result<Option<RecoveryCandidate>> {
        let segments = self
            .thread_store
            .load_canonical_artifact_segments(LoadThreadHistoryParams {
                thread_id,
                include_archived: false,
            })
            .await
            .map_err(io::Error::other)?;
        let Some(own_segment) = segments.segments.last() else {
            return Ok(None);
        };
        let Some(context) = select_unfinished_own_turn(thread_id, own_segment)? else {
            return Ok(None);
        };
        if own_segment.iter().any(|item| matches!(item,
            RolloutItem::SessionMeta(meta) if meta.meta.history_mode == ThreadHistoryMode::Paginated))
        {
            let page = self
                .thread_store
                .list_turns(ListTurnsParams {
                    thread_id,
                    include_archived: false,
                    cursor: None,
                    page_size: 1,
                    sort_direction: SortDirection::Desc,
                    items_view: StoredTurnItemsView::NotLoaded,
                })
                .await
                .map_err(io::Error::other)?;
            if !page.turns.first().is_some_and(|turn| {
                Some(&turn.turn_id) == context.turn_id.as_ref()
                    && turn.status == StoredTurnStatus::InProgress
            }) {
                return Ok(None);
            }
        }
        let model_context = self
            .thread_store
            .load_latest_model_context(LoadThreadHistoryParams {
                thread_id,
                include_archived: false,
            })
            .await
            .map_err(io::Error::other)?;
        if model_context.thread_id != thread_id
            || !model_context.items.iter().any(|item| {
                matches!(item,
                RolloutItem::TurnContext(saved) if saved == &context)
            })
        {
            return Err(io::Error::other(
                "unfinished turn has no recoverable model context",
            ));
        }
        let thread = self
            .thread_manager
            .get_thread(thread_id)
            .await
            .map_err(io::Error::other)?;
        let config = thread.config_snapshot().await;
        validate_recovery_settings(&context, &config)?;
        let fingerprint = serde_json::json!({
            "history": own_segment,
            "context": context,
            "environments": config.environment_selections().iter().map(ThreadEnvironment::from).collect::<Vec<_>>(),
            "permissions": config.permission_profile,
            "approvalPolicy": config.approval_policy,
            "approvalsReviewer": config.approvals_reviewer,
            "workspaceRoots": config.workspace_roots,
            "model": config.model,
            "collaborationMode": config.collaboration_mode,
            "reasoningEffort": config.reasoning_effort,
            "reasoningSummary": config.reasoning_summary,
            "personality": config.personality,
            "disabledPluginIds": config.disabled_plugin_ids,
            "serviceTier": config.service_tier,
        });
        Ok(Some(RecoveryCandidate {
            context,
            fingerprint,
        }))
    }
}

fn select_unfinished_own_turn(
    thread_id: ThreadId,
    segment: &[RolloutItem],
) -> io::Result<Option<TurnContextItem>> {
    let meta = segment
        .iter()
        .find_map(|item| match item {
            RolloutItem::SessionMeta(meta) => Some(&meta.meta),
            _ => None,
        })
        .ok_or_else(|| io::Error::other("canonical history lacks ownership metadata"))?;
    if meta.id != thread_id {
        return Err(io::Error::other(
            "canonical history belongs to another thread",
        ));
    }
    let own_start = match meta.subagent_history_start_ordinal {
        Some(ordinal) => usize::try_from(
            ordinal.saturating_sub(
                meta.history_base
                    .map_or(0, |base| base.end_ordinal_exclusive),
            ),
        )
        .map_err(io::Error::other)?,
        None if meta.parent_thread_id.is_some() || meta.forked_from_id.is_some() => segment
            .iter()
            .position(|item| {
                matches!(item,
                        RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(settings))
                            if settings.thread_id == Some(thread_id))
            })
            .ok_or_else(|| {
                io::Error::other("inherited history has no trustworthy own-turn boundary")
            })?,
        None => 0,
    };
    let own = segment
        .get(own_start..)
        .ok_or_else(|| io::Error::other("invalid own-turn boundary"))?;
    let Some(mut start) = own
        .iter()
        .rposition(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(_))))
    else {
        return Ok(None);
    };
    let RolloutItem::EventMsg(EventMsg::TurnStarted(started)) = &own[start] else {
        unreachable!("turn start selected above");
    };
    // Recovery records another start for the same logical turn, without another user message.
    // Retain that turn's original durable input, but never cross a different turn's boundary.
    while let Some(previous) = own[..start]
        .iter()
        .rposition(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(_))))
    {
        if !matches!(&own[previous], RolloutItem::EventMsg(EventMsg::TurnStarted(event))
            if event.turn_id == started.turn_id)
        {
            break;
        }
        start = previous;
    }
    let tail = &own[start..];
    if tail.iter().any(|item| match item {
        RolloutItem::EventMsg(EventMsg::TurnComplete(event)) => event.turn_id == started.turn_id,
        RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => event
            .turn_id
            .as_ref()
            .is_none_or(|id| id == &started.turn_id),
        RolloutItem::EventMsg(EventMsg::Error(event)) => event.affects_turn_status(),
        _ => false,
    }) {
        return Ok(None);
    }
    if tail.iter().any(|item| {
        matches!(
            item,
            RolloutItem::EventMsg(EventMsg::EnteredReviewMode(_))
                | RolloutItem::EventMsg(EventMsg::ExitedReviewMode(_))
                | RolloutItem::EventMsg(EventMsg::ThreadRolledBack(_))
                | RolloutItem::RealtimeItem(_)
        )
    }) {
        return Err(io::Error::other(
            "non-regular or rolled-back work requires explicit recovery",
        ));
    }
    let context = tail
        .iter()
        .rev()
        .find_map(|item| match item {
            RolloutItem::TurnContext(context)
                if context.turn_id.as_ref() == Some(&started.turn_id) =>
            {
                Some(context.clone())
            }
            _ => None,
        })
        .ok_or_else(|| io::Error::other("unfinished turn lacks its original settings"))?;
    if context.recovery_start_options.is_none() {
        return Err(io::Error::other(
            "original turn start options were not recorded; explicit retry is required",
        ));
    }
    let recipient = meta.agent_path.as_deref().or_else(|| {
        meta.parent_thread_id
            .is_none()
            .then_some(codex_protocol::AgentPath::ROOT)
    });
    let has_input = tail.iter().any(|item| match item {
        RolloutItem::ResponseItem(envelope) => match &envelope.item {
            ResponseItem::Message { role, content, .. } => role == "user" && !content.is_empty(),
            ResponseItem::AgentMessage {
                recipient: target,
                content,
                ..
            } => recipient == Some(target.as_str()) && !content.is_empty(),
            _ => false,
        },
        RolloutItem::InterAgentCommunication(message) => {
            recipient == Some(message.recipient.as_str())
                && (!message.content.is_empty() || message.encrypted_content.is_some())
        }
        _ => false,
    });
    if context.realtime_active == Some(true) || !has_input {
        return Err(io::Error::other(
            "unfinished turn lacks durable regular input",
        ));
    }
    validate_call_outcomes(tail)?;
    Ok(Some(context))
}

#[cfg(test)]
#[path = "local_tree_recovery_tests.rs"]
mod tests;
