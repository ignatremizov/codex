use super::attribution;
use crate::history_cell::AgentInputHistoryCell;
use crate::history_cell::HistoryCell;
use crate::test_support::PathBufExt;
use crate::test_support::test_path_buf;
use codex_app_server_protocol::ThreadItemPayload;
use codex_protocol::ThreadId;
use codex_protocol::models::MessagePhase;
use pretty_assertions::assert_eq;
use ratatui::style::Modifier;

#[test]
fn message_body_tint_follows_recipient_and_mail_acceptance() -> anyhow::Result<()> {
    let attribution = attribution();
    let recipient = ThreadId::from_string(&attribution.recipient.thread_id)?;
    let receipt =
        codex_protocol::mailbox_acceptance_receipt_id(attribution.sender.thread_id.as_str())
            .ok_or_else(|| anyhow::anyhow!("mail receipt identity"))?;
    let mut snapshot = Vec::new();
    for (label, viewed, receipt_id, expected_dim) in [
        ("recipient", Some(recipient), "input", false),
        ("peer copy", Some(ThreadId::new()), "input", true),
        ("unknown view", None, "input", false),
        ("unconsumed mail", Some(recipient), receipt.as_str(), true),
    ] {
        let cell = AgentInputHistoryCell::new(
            attribution.clone(),
            Vec::new(),
            "Keep the original payload.".to_string(),
            viewed,
        )
        .with_receipt_id(receipt_id);
        let display = cell.display_lines(/*width*/ 200);
        let transcript = cell.transcript_lines(/*width*/ 200);
        assert_eq!(&transcript[..display.len()], display);
        let payload = display
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content == "Keep the original payload.")
            .ok_or_else(|| anyhow::anyhow!("rendered payload"))?;
        assert_eq!(
            payload.style.add_modifier.contains(Modifier::DIM),
            expected_dim,
            "{label}"
        );
        let sender = display
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content == "Pascal")
            .ok_or_else(|| anyhow::anyhow!("sender heading"))?;
        assert_eq!(
            (
                sender.style.add_modifier.contains(Modifier::BOLD),
                sender.style.add_modifier.contains(Modifier::DIM),
            ),
            (true, false)
        );
        snapshot.push(format!("{label}: {payload:?}"));
        assert_eq!(
            cell.raw_lines()[1].to_string(),
            "Keep the original payload."
        );
    }
    insta::assert_snapshot!(snapshot.join("\n"), @r#"
    recipient: Span::from("Keep the original payload.")
    peer copy: Span::from("Keep the original payload.").dim()
    unknown view: Span::from("Keep the original payload.")
    unconsumed mail: Span::from("Keep the original payload.").dim()
    "#);
    Ok(())
}

#[test]
fn replay_keeps_the_same_recipient_relative_tint_as_live_cells() -> anyhow::Result<()> {
    let attribution = attribution();
    let recipient = ThreadId::from_string(&attribution.recipient.thread_id)?;
    for viewed in [Some(recipient), Some(ThreadId::new()), None] {
        let payload = "Original message\nkept intact across wrapping and replay.";
        let item = ThreadItemPayload::AgentMessage {
            id: "input".to_string(),
            text: payload.to_string(),
            attribution: Some(attribution.clone()),
            input: None,
            inter_agent_source: None,
            phase: Some(MessagePhase::Commentary),
            memory_citation: None,
            delivery: None,
            questions: None,
        };
        let projected = crate::thread_transcript::thread_items_to_transcript_cells(
            viewed,
            &test_path_buf("/workspace").abs(),
            [item],
            crate::thread_transcript::RawReasoningVisibility::Hidden,
            /*config*/ None,
        );
        let live = AgentInputHistoryCell::new(
            attribution.clone(),
            Vec::new(),
            payload.to_string(),
            viewed,
        );
        assert_eq!(projected.len(), 1);
        for width in [32, 200] {
            assert_eq!(projected[0].display_lines(width), live.display_lines(width));
            assert_eq!(
                projected[0].transcript_lines(width),
                live.transcript_lines(width)
            );
        }
        assert_eq!(projected[0].raw_lines(), live.raw_lines());
    }
    Ok(())
}
