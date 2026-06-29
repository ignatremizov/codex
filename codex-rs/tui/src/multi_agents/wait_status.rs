//! Compact live-wait labels, sharing the existing agent display metadata.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WaitStatusSummary {
    pub(crate) header: String,
    pub(crate) details: Option<String>,
    pub(crate) details_max_lines: usize,
}

pub(crate) fn wait_status_summary(
    receiver_thread_ids: &[String],
    agent_metadata: &mut impl FnMut(ThreadId) -> AgentMetadata,
) -> WaitStatusSummary {
    let receiver_agents = receiver_thread_ids
        .iter()
        .filter_map(|thread_id| parse_thread_id(thread_id))
        .map(|thread_id| (thread_id, agent_metadata(thread_id)))
        .collect::<Vec<_>>();

    let header = match receiver_agents.as_slice() {
        [(thread_id, metadata)] => {
            format!(
                "Waiting for {}",
                agent_label_plain(agent_label(*thread_id, metadata))
            )
        }
        [] => "Waiting for agents".to_string(),
        _ => format!("Waiting for {} agents", receiver_agents.len()),
    };

    let details = (receiver_agents.len() > 1).then(|| {
        receiver_agents
            .iter()
            .map(|(thread_id, metadata)| agent_label_plain(agent_label(*thread_id, metadata)))
            .collect::<Vec<_>>()
            .join("\n")
    });
    let details_max_lines = if receiver_agents.len() > 1 { 3 } else { 1 };

    WaitStatusSummary {
        header,
        details,
        details_max_lines,
    }
}

fn agent_label_plain(agent: AgentLabel<'_>) -> String {
    let nickname = agent
        .nickname
        .map(str::trim)
        .filter(|nickname| !nickname.is_empty());
    let role = agent.role.map(str::trim).filter(|role| !role.is_empty());
    match (nickname, role, agent.thread_id) {
        (Some(nickname), Some(role), _) => format!("{nickname} [{role}]"),
        (Some(nickname), None, _) => nickname.to_string(),
        (None, Some(role), _) => format!("[{role}]"),
        (None, None, Some(thread_id)) => thread_id.to_string(),
        (None, None, None) => "agent".to_string(),
    }
}
