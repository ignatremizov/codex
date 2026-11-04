//! Completion snapshots must render without a preceding spawn or navigation cache entry.

use super::*;
use crate::exec_cell::OutputPreviewLineLimits;
use crate::thread_transcript::RawReasoningVisibility;
use crate::thread_transcript::thread_items_to_transcript_cells_with_preview_line_limits;
use codex_protocol::AgentInputAttribution;
use codex_protocol::AgentInputIdentity;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SubAgentCompletionModelVisibility;
use codex_protocol::protocol::agent_delivery_receipt_item;
use codex_protocol::protocol::new_sub_agent_completion_context_response_item_id;
use codex_protocol::protocol::sub_agent_completion_item_with_visibility;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn cold_grandchild_labels_and_confirmed_recipient_match_live_raw_and_replay() {
    let sender = ThreadId::new();
    let recipient = ThreadId::new();
    let attribution = AgentInputAttribution {
        sender: AgentInputIdentity {
            thread_id: sender,
            nickname: Some("Archimedes".into()),
            agent_ref: Some("34".into()),
            task_path: Some("/root/performance/scout".into()),
            role: Some("explorer".into()),
            model: Some("gpt-6.1-sol".into()),
            reasoning_effort: Some(ReasoningEffortConfig::High),
        },
        recipient: AgentInputIdentity {
            thread_id: recipient,
            nickname: Some("Averroes".into()),
            agent_ref: Some("31".into()),
            task_path: Some("/root/performance".into()),
            role: Some("coder".into()),
            model: Some("parent-model".into()),
            reasoning_effort: None,
        },
        sender_turn_id: "scout-turn".into(),
        batch_id: None,
    };
    // A native path in the body must resolve through trusted attribution, too.
    let mut completion = sub_agent_completion_item_with_visibility(
        "/root/performance/scout",
        &AgentStatus::Completed(Some("The harness is the bottleneck.".into())),
        SubAgentCompletionModelVisibility::NotVisible,
    )
    .expect("completion");
    completion.attribution = Some(attribution.clone());
    let mut receipt = agent_delivery_receipt_item(
        sender,
        recipient,
        MessagePhase::FinalAnswer,
        SubAgentCompletionModelVisibility::Visible,
        new_sub_agent_completion_context_response_item_id().as_str(),
        "The harness is the bottleneck.",
    )
    .expect("receipt");
    receipt.attribution = Some(attribution);
    let mut rows = Vec::new();
    for core_item in [completion, receipt] {
        let item = AppServerThreadItemPayload::from(TurnItem::AgentMessage(core_item.clone()));
        let (mut chat, mut rx, _operations) = make_chatwidget_manual(/*model_override*/ None).await;
        chat.handle_server_notification(
            ServerNotification::ItemCompleted(ItemCompletedNotification::new(
                "root-thread".into(),
                "root-turn".into(),
                item.clone(),
                /*started_at_ms*/ None,
                /*completed_at_ms*/ 0,
            )),
            /*replay_kind*/ None,
        );
        chat.replay_thread_item(
            item.clone(),
            "root-turn".into(),
            ReplayKind::ResumeInitialMessages,
        );
        chat.on_agent_message_item_completed(core_item, "root-turn", /*from_replay*/ false);
        let live = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| match event {
                AppEvent::InsertHistoryCell(cell) => Some(cell.raw_lines()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let cold = thread_items_to_transcript_cells_with_preview_line_limits(
            /*thread_id*/ None,
            &chat.config.cwd,
            [item],
            RawReasoningVisibility::Hidden,
            Some(&chat.config),
            OutputPreviewLineLimits::default(),
            (&chat.local_settings.tui).into(),
        );
        assert_eq!((live.len(), cold.len()), (3, 1));
        assert_eq!(live, vec![cold[0].raw_lines(); 3]);
        rows.push(lines_to_single_string(&live[0]));
    }
    assert_snapshot!(
        rows.join("\n"),
        @r"
    • Archimedes [explorer] /root/performance/scout (34) (gpt-6.1-sol high) completed: (○ not visible)
      └ The harness is the bottleneck.

    • Archimedes (34) · final delivered to Averroes (31)
    "
    );
}
