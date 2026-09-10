use super::*;

#[test]
fn completed_resume_reports_readiness_without_replaying_the_final() {
    let thread_id = ThreadId::new();
    for message in [None, Some("previous final".to_string())] {
        let state = CollabAgentState {
            status: CollabAgentStatus::Completed,
            message,
        };
        let cell = resume_end(
            thread_id,
            Some(&state),
            "resume failed",
            V1ResponseObservation {
                observe_commentary: Some(false),
                wake_on_completion: Some(false),
                target_messages: Some(false),
                queue_input: Some(false),
            },
            &mut |_| AgentMetadata {
                agent_nickname: Some("Mill".to_string()),
                ..Default::default()
            },
        );
        let text = cell
            .display_lines(/*width*/ 120)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        insta::allow_duplicates! {
            insta::assert_snapshot!(
                text,
                @r"
            • Resumed Mill (no commentary · no wake on completion)
              └ Idle
            "
            );
        }
    }
}
