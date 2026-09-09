use super::*;
use pretty_assertions::assert_eq;

#[test]
fn lifecycle_labels_include_authoritative_task_path() {
    let thread_id = ThreadId::new();
    let metadata = AgentMetadata {
        agent_nickname: Some("Darwin".to_string()),
        agent_role: Some("worker".to_string()),
        task_path: AgentTaskPath::Known(Some("/root/mailbox-test".to_string())),
        spawn_request: Some(SpawnRequestSummary {
            model: Some("gpt-5".to_string()),
            reasoning_effort: Some(ReasoningEffortConfig::High),
        }),
    };
    let label = agent_label(thread_id, &metadata);
    let plain = agent_label_plain(label);
    let styled = agent_label_line(label);
    assert_eq!(plain, styled.to_string());
    insta::assert_snapshot!(plain, @"Darwin [worker] (gpt-5 high) /root/mailbox-test");
    let title = title_with_agent("Spawned", label, /*spawn_request*/ None).to_string();
    insta::assert_snapshot!(title, @"• Spawned Darwin [worker] (gpt-5 high) /root/mailbox-test");
    assert_eq!(styled.spans.last(), Some(&" /root/mailbox-test".dim()));
}

#[test]
fn blank_task_path_does_not_add_a_suffix() {
    let metadata = AgentMetadata {
        agent_nickname: Some("Darwin".to_string()),
        task_path: AgentTaskPath::Known(Some(" \t".to_string())),
        ..Default::default()
    };
    let label = agent_label(ThreadId::new(), &metadata);
    assert_eq!(agent_label_plain(label), "Darwin");
    assert_eq!(agent_label_line(label).to_string(), "Darwin");
}

#[test]
fn metadata_refresh_preserves_unknown_paths_but_applies_authoritative_clears() {
    let thread_id = ThreadId::new();
    let cell = CollabAgentHistoryCell::new_agent_labeled(
        thread_id,
        &AgentMetadata {
            agent_nickname: Some("Darwin".to_string()),
            task_path: AgentTaskPath::Known(Some("/root/mailbox-test".to_string())),
            ..Default::default()
        },
        vec![" completed".into()],
        Vec::new(),
    );
    assert!(
        cell.with_refreshed_agent_metadata(|_| Some(AgentMetadata::default()))
            .is_none()
    );
    let cleared = cell
        .with_refreshed_agent_metadata(|_| {
            Some(AgentMetadata {
                task_path: AgentTaskPath::Known(None),
                ..Default::default()
            })
        })
        .expect("authoritative clear changes the title");
    let rendered = cleared
        .transcript_lines(/*width*/ 120)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @"• Darwin completed");
}
