use super::*;
use crate::history_cell::HistoryCell;
use codex_app_server_protocol::CollabAgentTool;
use codex_app_server_protocol::ThreadItem;
use pretty_assertions::assert_eq;
use std::collections::HashMap;

#[test]
fn mailbox_send_has_distinct_acceptance_and_failure_titles() {
    let receiver = ThreadId::new();
    let mut item = ThreadItem::CollabAgentToolCall {
        id: "mail-send".into(),
        tool: CollabAgentTool::SendInput,
        status: CollabAgentToolCallStatus::Completed,
        observe_commentary: Some(false),
        wake_on_completion: Some(false),
        target_messages: Some(false),
        queue_input: Some(false),
        mailbox_input: Some(true),
        sender_thread_id: ThreadId::new().to_string(),
        receiver_thread_ids: vec![receiver.to_string()],
        receiver_agents: Vec::new(),
        prompt: Some("Read these notes when ready.".into()),
        model: None,
        reasoning_effort: None,
        agents_states: HashMap::new(),
    };
    let render = |item: &ThreadItem| {
        super::super::tool_call_history_cell(
            item,
            /*cached_spawn_request*/ None,
            /*agent_prompt_preview_lines*/ 0,
            /*agent_response_preview_lines*/ 0,
            |_| AgentMetadata {
                agent_nickname: Some("Darwin".into()),
                ..Default::default()
            },
        )
        .expect("send renders")
        .display_lines(/*width*/ 100)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
    };
    insta::assert_snapshot!(render(&item), @"
    • Saved to mailbox for Darwin
      └ Read these notes when ready.
    ");
    if let ThreadItem::CollabAgentToolCall { status, .. } = &mut item {
        *status = CollabAgentToolCallStatus::Failed;
    }
    insta::assert_snapshot!(render(&item), @"
    • Failed to save to mailbox for Darwin
      └ Read these notes when ready.
    ");
    if let ThreadItem::CollabAgentToolCall {
        status,
        mailbox_input,
        ..
    } = &mut item
    {
        *status = CollabAgentToolCallStatus::Completed;
        *mailbox_input = None;
    }
    assert!(render(&item).contains("Sent input to Darwin"));
    assert!(!render(&item).contains("mailbox"));
}

#[test]
fn mailbox_preview_preserves_full_transcript() {
    let cell = history_cell(
        ThreadId::new(),
        CollabAgentToolCallStatus::Completed,
        "First line\nSecond line\nThird line",
        /*preview_lines*/ 1,
        &mut |_| AgentMetadata {
            agent_nickname: Some("Darwin".into()),
            ..Default::default()
        },
    );
    let full = cell
        .transcript_lines(/*width*/ 100)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(full, @"
    • Saved to mailbox for Darwin
      └ First line
        Second line
        Third line
    ");
    assert_eq!(
        cell.display_lines(/*width*/ 100)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec!["• Saved to mailbox for Darwin", "  └ … +3 rows hidden",],
    );
}
