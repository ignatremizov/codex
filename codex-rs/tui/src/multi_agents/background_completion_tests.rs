use super::*;
use crate::history_cell::HistoryCell;
use codex_protocol::ThreadId;
use codex_protocol::protocol::agent_delivery_receipt_item;
use codex_protocol::protocol::new_sub_agent_completion_context_response_item_id;
use pretty_assertions::assert_eq;

#[test]
fn receipt_preserves_direction_preview_transcript_and_metadata_refresh() {
    let sender = ThreadId::new();
    let recipient = ThreadId::new();
    let payload = "Use API revision 2.\nThe field is optional.";
    for (phase, label, visibility, visibility_label) in [
        (
            MessagePhase::Commentary,
            "commentary",
            SubAgentCompletionModelVisibility::Visible,
            "● visible",
        ),
        (
            MessagePhase::FinalAnswer,
            "final",
            SubAgentCompletionModelVisibility::Visible,
            "● visible",
        ),
        (
            MessagePhase::FinalAnswer,
            "final",
            SubAgentCompletionModelVisibility::NotVisible,
            "○ not visible",
        ),
    ] {
        let receipt = agent_delivery_receipt_item(
            sender,
            recipient,
            phase,
            visibility,
            new_sub_agent_completion_context_response_item_id().as_str(),
            payload,
        )
        .expect("receipt");
        let cell = background_completion_history_cell_from_agent_message(
            &receipt.id,
            payload,
            receipt.phase.as_ref(),
            /*agent_response_preview_lines*/ 1,
            |id| AgentMetadata {
                agent_nickname: (id == recipient).then(|| "Newton".to_string()),
                agent_role: Some("default".to_string()),
                ..Default::default()
            },
        )
        .expect("receipt cell");
        let rendered = cell
            .display_lines(/*width*/ 120)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let transcript = cell
            .transcript_lines(/*width*/ 120)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        if label == "final" && visibility == SubAgentCompletionModelVisibility::NotVisible {
            insta::assert_snapshot!(rendered, @r"
            • Main [default] → Newton [default] · final delivered (○ not visible to recipient):
              └ … +2 rows hidden
            ");
            insta::assert_snapshot!(transcript, @r"
            • Main [default] → Newton [default] · final delivered (○ not visible to recipient):
              └ Use API revision 2.
                The field is optional.
            ");
        } else if label == "final" {
            insta::assert_snapshot!(rendered, @r"
            • Main [default] → Newton [default] · final delivered (● visible to recipient):
              └ … +2 rows hidden
            ");
            insta::assert_snapshot!(transcript, @r"
            • Main [default] → Newton [default] · final delivered (● visible to recipient):
              └ Use API revision 2.
                The field is optional.
            ");
        } else {
            insta::assert_snapshot!(rendered, @r"
            • Main [default] → Newton [default] · commentary delivered (● visible to recipient):
              └ … +2 rows hidden
            ");
            insta::assert_snapshot!(transcript, @r"
            • Main [default] → Newton [default] · commentary delivered (● visible to recipient):
              └ Use API revision 2.
                The field is optional.
            ");
        }
        let refreshed = cell
            .with_refreshed_agent_metadata(|id| {
                (id == recipient).then(|| AgentMetadata {
                    agent_nickname: Some("Curie".to_string()),
                    ..Default::default()
                })
            })
            .expect("recipient refresh");
        assert_eq!(
            refreshed.display_lines(/*width*/ 120)[0].to_string(),
            format!(
                "• Main [default] → Curie [default] · {label} delivered ({visibility_label} to recipient):"
            )
        );
        assert!(
            background_completion_history_cell_from_agent_message(
                &receipt.id,
                payload,
                Some(&MessagePhase::FinalAnswer),
                /*agent_response_preview_lines*/ 0,
                |_| AgentMetadata::default(),
            )
            .is_none()
        );
    }
}
