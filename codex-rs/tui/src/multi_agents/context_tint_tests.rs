use super::*;
use crate::history_cell::HistoryCell;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SubAgentCompletionModelVisibility;
use codex_protocol::protocol::agent_delivery_receipt_item;
use codex_protocol::protocol::new_attributed_agent_message_response_item_id;
use codex_protocol::protocol::new_sub_agent_completion_context_response_item_id;
use codex_protocol::protocol::sub_agent_completion_item_with_visibility;
use pretty_assertions::assert_eq;
use ratatui::style::Modifier;

#[test]
fn completion_and_receipt_bodies_distinguish_the_viewed_threads_context() -> anyhow::Result<()> {
    let sender = ThreadId::new();
    let recipient = ThreadId::new();
    let payload = "Result https://example.com/report\nComplete second line";
    let mut snapshot = Vec::new();
    for visibility in [
        SubAgentCompletionModelVisibility::Visible,
        SubAgentCompletionModelVisibility::NotVisible,
    ] {
        let completion = sub_agent_completion_item_with_visibility(
            &sender.to_string(),
            &AgentStatus::Completed(Some(payload.to_string())),
            visibility,
        )
        .ok_or_else(|| anyhow::anyhow!("completion item"))?;
        let receipt = agent_delivery_receipt_item(
            sender,
            recipient,
            MessagePhase::FinalAnswer,
            visibility,
            new_sub_agent_completion_context_response_item_id().as_str(),
            payload,
        )
        .ok_or_else(|| anyhow::anyhow!("receipt"))?;
        for (kind, item, text, expected_dim) in [
            (
                "completion",
                &completion,
                format!("Agent final answer from `{sender}`:\n\n{payload}"),
                visibility == SubAgentCompletionModelVisibility::NotVisible,
            ),
            ("receipt", &receipt, payload.to_string(), true),
        ] {
            let cell = background_completion_history_cell_from_agent_message(
                &item.id,
                &text,
                item.phase.as_ref(),
                /*attribution*/ None,
                /*agent_response_preview_lines*/ 2,
                |_| AgentMetadata::default(),
            )
            .ok_or_else(|| anyhow::anyhow!("completion cell"))?;
            let transcript = cell.transcript_hyperlink_lines(/*width*/ 120);
            let first_body = &transcript[1];
            assert_eq!(
                first_body.line.style.add_modifier.contains(Modifier::DIM),
                expected_dim
            );
            assert!(
                !transcript[0]
                    .line
                    .style
                    .add_modifier
                    .contains(Modifier::DIM)
            );
            assert!(!first_body.hyperlinks.is_empty());
            snapshot.push(format!(
                "{kind} {visibility:?}: {:?}",
                first_body
                    .line
                    .spans
                    .iter()
                    .find(|span| span.content.contains("Result"))
                    .ok_or_else(|| anyhow::anyhow!("rendered body"))?,
            ));
            for width in [20, 120] {
                let display = cell.display_lines(width);
                let full = cell.transcript_lines(width);
                assert!(display.len() <= 3);
                assert!(full.iter().skip(1).all(|line| {
                    line.style.add_modifier.contains(Modifier::DIM) == expected_dim
                }));
                assert_eq!(
                    cell.raw_lines()
                        .iter()
                        .skip(1)
                        .map(ToString::to_string)
                        .collect::<Vec<_>>(),
                    vec![
                        "  └ Result https://example.com/report".to_string(),
                        "    Complete second line".to_string(),
                    ]
                );
            }
        }
    }
    insta::assert_snapshot!(snapshot.join("\n"), @r#"
    completion Visible: Span::from("Result https://example.com/report")
    receipt Visible: Span::from("Result https://example.com/report").dim()
    completion NotVisible: Span::from("Result https://example.com/report").dim()
    receipt NotVisible: Span::from("Result https://example.com/report").dim()
    "#);
    Ok(())
}

#[test]
fn peer_audit_tint_survives_identity_refresh_and_hyperlink_wrapping() -> anyhow::Result<()> {
    let sender = ThreadId::new();
    let recipient = ThreadId::new();
    let cell = background_commentary_history_cell_from_agent_message(
        new_attributed_agent_message_response_item_id().as_str(),
        &format!(
            "Agent message from `{sender}` to `{recipient}`:\n\nUse https://example.com/report"
        ),
        Some(&MessagePhase::Commentary),
        /*agent_response_preview_lines*/ 0,
        |_| AgentMetadata::default(),
    )
    .ok_or_else(|| anyhow::anyhow!("audit cell"))?;
    let refreshed = cell
        .with_refreshed_agent_metadata(|id| {
            (id == sender).then(|| AgentMetadata {
                agent_nickname: Some("Curie".to_string()),
                ..Default::default()
            })
        })
        .ok_or_else(|| anyhow::anyhow!("refreshed heading"))?;
    for rendered in [cell, refreshed] {
        let lines = rendered.transcript_hyperlink_lines(/*width*/ 120);
        assert!(lines[1].line.style.add_modifier.contains(Modifier::DIM));
        assert!(!lines[1].hyperlinks.is_empty());
        assert_eq!(
            rendered
                .raw_lines()
                .iter()
                .skip(1)
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["  └ Use https://example.com/report".to_string()]
        );
    }
    Ok(())
}
