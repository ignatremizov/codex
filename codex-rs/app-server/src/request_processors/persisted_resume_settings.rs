use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::models::ActivePermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use codex_rollout::InitialHistory;
use codex_rollout::RolloutItem;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct PersistedResumeSettings {
    pub(super) approval_policy: AskForApproval,
    pub(super) approvals_reviewer: Option<ApprovalsReviewer>,
    pub(super) active_permission_profile: Option<ActivePermissionProfile>,
}

pub(super) fn latest_persisted_resume_settings(
    history: &[RolloutItem],
) -> Option<PersistedResumeSettings> {
    history
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, item)| match item {
            RolloutItem::TurnContext(turn_context) => {
                let updated_settings = turn_context.turn_id.as_ref().and_then(|turn_id| {
                    let turn_start = history[..index].iter().rposition(|item| {
                        matches!(
                            item,
                            RolloutItem::EventMsg(EventMsg::TurnStarted(event))
                                if &event.turn_id == turn_id
                        )
                    })?;
                    history[turn_start + 1..index]
                        .iter()
                        .rev()
                        .find_map(|item| match item {
                            RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event)) => {
                                Some(&event.thread_settings)
                            }
                            _ => None,
                        })
                });
                Some(PersistedResumeSettings {
                    approval_policy: updated_settings
                        .map_or(turn_context.approval_policy, |settings| {
                            settings.approval_policy
                        }),
                    approvals_reviewer: updated_settings.map_or_else(
                        || {
                            turn_context.approvals_reviewer.or_else(|| {
                                history[..index].iter().rev().find_map(|item| match item {
                                    RolloutItem::TurnContext(turn_context) => {
                                        turn_context.approvals_reviewer
                                    }
                                    RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(
                                        event,
                                    )) => Some(event.thread_settings.approvals_reviewer),
                                    _ => None,
                                })
                            })
                        },
                        |settings| Some(settings.approvals_reviewer),
                    ),
                    active_permission_profile: updated_settings.map_or_else(
                        || turn_context.active_permission_profile.clone(),
                        |settings| settings.active_permission_profile.clone(),
                    ),
                })
            }
            RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event)) => {
                Some(PersistedResumeSettings {
                    approval_policy: event.thread_settings.approval_policy,
                    approvals_reviewer: Some(event.thread_settings.approvals_reviewer),
                    active_permission_profile: event
                        .thread_settings
                        .active_permission_profile
                        .clone(),
                })
            }
            _ => None,
        })
}

pub(super) fn latest_persisted_model_settings(
    history: &InitialHistory,
) -> Option<&ThreadSettingsSnapshot> {
    let InitialHistory::Resumed(history) = history else {
        return None;
    };
    let mut crossed_foreign_snapshot = false;
    for item in history.history.iter().rev() {
        let RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event)) = item else {
            continue;
        };
        match event.thread_id {
            Some(thread_id) if thread_id == history.conversation_id => {
                return Some(&event.thread_settings);
            }
            Some(_) => crossed_foreign_snapshot = true,
            None if !crossed_foreign_snapshot => return Some(&event.thread_settings),
            None => {}
        }
    }
    None
}

#[cfg(test)]
#[path = "persisted_resume_settings_tests.rs"]
mod tests;
