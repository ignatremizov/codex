use super::*;
use codex_app_server_protocol::AgentAlias;
use pretty_assertions::assert_eq;

#[test]
fn relative_lookup_and_completion_follow_issuing_thread_and_exclude_transferred() {
    let main = ThreadId::new();
    let backend = ThreadId::new();
    let review = ThreadId::new();
    let root_review = ThreadId::new();
    let transferred = ThreadId::new();
    let mut state = AgentNavigationState::default();
    state.replace_aliases(
        [
            (main, "1", None, AgentAliasState::Active),
            (backend, "2", Some("/root/backend"), AgentAliasState::Active),
            (
                review,
                "3",
                Some("/root/backend/review"),
                AgentAliasState::Closed,
            ),
            (
                root_review,
                "4",
                Some("/root/review"),
                AgentAliasState::Active,
            ),
            (
                transferred,
                "5",
                Some("/root/backend/old"),
                AgentAliasState::Transferred,
            ),
        ]
        .into_iter()
        .map(|(id, agent_ref, task_path, state)| AgentAlias {
            thread_id: id.to_string(),
            agent_ref: agent_ref.to_string(),
            nickname: None,
            task_path: task_path.map(str::to_string),
            state,
        })
        .collect(),
    );
    assert_eq!(
        [
            state.thread_id_for_task_path(Some(backend), "review"),
            state.thread_id_for_task_path(Some(backend), "/root/review"),
            state.thread_id_for_task_path(Some(main), "review"),
            state.thread_id_for_task_path(None, "review"),
            state.thread_id_for_task_path(Some(backend), "old"),
        ],
        [
            Some(review),
            Some(root_review),
            Some(root_review),
            Some(root_review),
            None
        ],
    );
    let target = AgentPromptTarget {
        thread_id: Some(review),
        selector: "3".to_string(),
        label: "Reviewer /root/backend/review · closed".to_string(),
    };
    assert_eq!(
        state.task_path_completions(Some(backend), std::slice::from_ref(&target)),
        vec![
            AgentPromptTarget {
                selector: "task:/root/backend/review".to_string(),
                ..target.clone()
            },
            AgentPromptTarget {
                selector: "task:review".to_string(),
                ..target
            },
        ],
    );
}
