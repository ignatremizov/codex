use super::*;
use crate::history_cell::HistoryCell;
use codex_protocol::ThreadId;
use codex_protocol::protocol::new_attributed_agent_message_response_item_id;
use pretty_assertions::assert_eq;

#[test]
fn peer_message_audit_renders_both_endpoints_and_visibility() {
    let sender = ThreadId::new();
    let recipient = ThreadId::new();
    let text = format!(
        "Agent message from `{sender}` to `{recipient}`:\n\nUse API revision 2.\nThe field is optional."
    );
    let cell = background_commentary_history_cell_from_agent_message(
        new_attributed_agent_message_response_item_id().as_str(),
        &text,
        Some(&MessagePhase::Commentary),
        /*agent_response_preview_lines*/ 1,
        |id| AgentMetadata {
            agent_nickname: Some(if id == sender { "Banach" } else { "Franklin" }.to_string()),
            agent_role: Some("coder".to_string()),
            ..Default::default()
        },
    )
    .expect("audit cell");
    let rendered = cell
        .display_lines(/*width*/ 120)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @r"
    • Banach [coder] sends to Franklin [coder] (○ not visible):
      └ … +2 rows hidden
    ");
    let transcript = cell
        .transcript_lines(/*width*/ 120)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(transcript, @r"
    • Banach [coder] sends to Franklin [coder] (○ not visible):
      └ Use API revision 2.
        The field is optional.
    ");
    assert!(
        background_commentary_history_cell_from_agent_message(
            "ordinary-model-message",
            &text,
            Some(&MessagePhase::Commentary),
            /*agent_response_preview_lines*/ 0,
            |_| AgentMetadata::default(),
        )
        .is_none()
    );

    let renamed = cell
        .with_refreshed_agent_metadata(|id| {
            (id == recipient).then(|| AgentMetadata {
                agent_nickname: Some("Curie".to_string()),
                ..Default::default()
            })
        })
        .expect("recipient-only metadata refresh");
    assert_eq!(
        renamed.display_lines(/*width*/ 120)[0].to_string(),
        "• Banach [coder] sends to Curie [coder] (○ not visible):"
    );
}
