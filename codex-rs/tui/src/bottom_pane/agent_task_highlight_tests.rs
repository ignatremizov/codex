use super::*;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

#[test]
fn paths_match_exact_case_and_resume_labels_are_options() {
    let targets = vec![AgentPromptTarget {
        thread_id: Some(ThreadId::new()),
        selector: "task:/root/Backend".to_string(),
        label: "Pascal [coder] /root/Backend".to_string(),
    }];
    for (input, expected) in [
        (
            "/agent task:/root/Backend",
            AgentCommandHighlightKind::KnownTarget,
        ),
        (
            "/agent /root/Backend",
            AgentCommandHighlightKind::KnownTarget,
        ),
        (
            "/agent task:/root/backend",
            AgentCommandHighlightKind::UnknownTarget,
        ),
    ] {
        assert_eq!(
            agent_command_highlights(input, &targets, &[]),
            vec![
                AgentCommandHighlight {
                    range: 0..6,
                    kind: AgentCommandHighlightKind::Command,
                },
                AgentCommandHighlight {
                    range: 7..input.len(),
                    kind: expected,
                },
            ],
        );
    }
    let input = "/agent /root/Backend resume task:import";
    assert_eq!(
        agent_command_highlights(input, &targets, &[]).get(2),
        Some(&AgentCommandHighlight {
            range: 21..27,
            kind: AgentCommandHighlightKind::Action,
        }),
    );
    assert_eq!(
        agent_command_highlights(input, &targets, &[]).last(),
        Some(&AgentCommandHighlight {
            range: 28..39,
            kind: AgentCommandHighlightKind::Option,
        }),
    );
    let literal = "/agent /root/Backend -- resume the review";
    assert_eq!(agent_command_highlights(literal, &targets, &[]).len(), 2);
}
