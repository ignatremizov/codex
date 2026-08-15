use super::*;

pub(super) fn resolve_without_alias_store(
    target: V1AgentTarget,
    scope: V1AgentTargetScope,
    process_local_controlled: bool,
    root_thread_id: Option<ThreadId>,
) -> CodexResult<ThreadId> {
    if let V1AgentTarget::Nickname(nickname) = &target
        && nickname.eq_ignore_ascii_case(MAIN_AGENT_NICKNAME)
        && let Some(root_thread_id) = root_thread_id
    {
        return Ok(root_thread_id);
    }
    match (target, scope) {
        (V1AgentTarget::Id(thread_id), V1AgentTargetScope::AllowUuidAdoption) => Ok(thread_id),
        (V1AgentTarget::Id(thread_id), V1AgentTargetScope::ControlledOnly)
            if process_local_controlled =>
        {
            Ok(thread_id)
        }
        (V1AgentTarget::Id(thread_id), V1AgentTargetScope::ControlledOnly) => {
            Err(CodexErr::UnsupportedOperation(format!(
                "agent {thread_id} is not controlled by this root; use resume_agent to adopt it"
            )))
        }
        (V1AgentTarget::Ref(_) | V1AgentTarget::Nickname(_), _) => {
            Err(CodexErr::UnsupportedOperation(
                "short agent targets are unavailable; use the full agent UUID".to_string(),
            ))
        }
    }
}

pub(super) fn parse_v1_agent_target(target: &str) -> CodexResult<V1AgentTarget> {
    if let Some(thread_id) = target.strip_prefix("id:") {
        return ThreadId::from_string(thread_id)
            .map(V1AgentTarget::Id)
            .map_err(|err| {
                CodexErr::UnsupportedOperation(format!("invalid agent UUID {thread_id:?}: {err}"))
            });
    }
    if let Some(agent_ref) = target.strip_prefix("ref:") {
        return parse_v1_agent_ref(agent_ref).map(V1AgentTarget::Ref);
    }
    if let Some(nickname) = target.strip_prefix("nick:")
        && nickname.is_empty()
    {
        return Err(CodexErr::UnsupportedOperation(
            "agent nickname cannot be empty".to_string(),
        ));
    }
    if let Some(nickname) = target.strip_prefix("nick:") {
        return Ok(V1AgentTarget::Nickname(nickname.to_string()));
    }
    if let Ok(thread_id) = ThreadId::from_string(target) {
        return Ok(V1AgentTarget::Id(thread_id));
    }
    if target.is_empty() {
        return Err(CodexErr::UnsupportedOperation(
            "agent target cannot be empty".to_string(),
        ));
    }
    if target.chars().all(|ch| ch.is_ascii_digit()) {
        return parse_v1_agent_ref(target).map(V1AgentTarget::Ref);
    }
    Ok(V1AgentTarget::Nickname(target.to_string()))
}

fn parse_v1_agent_ref(agent_ref: &str) -> CodexResult<u64> {
    let agent_ref = agent_ref.parse::<u64>().map_err(|err| {
        CodexErr::UnsupportedOperation(format!("invalid agent ref {agent_ref:?}: {err}"))
    })?;
    if agent_ref == 0 {
        return Err(CodexErr::UnsupportedOperation(
            "agent refs start at 1".to_string(),
        ));
    }
    Ok(agent_ref)
}
