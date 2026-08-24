use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::CommandExecutionItem;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::new_attributed_agent_message_response_item_id;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use pretty_assertions::assert_eq;
use std::time::Duration;

use super::should_persist_event_msg;

#[test]
fn attributed_agent_input_presentation_is_persisted_in_every_history_mode() {
    let event = EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: codex_protocol::ThreadId::new(),
        turn_id: "turn-1".to_string(),
        item: TurnItem::AgentMessage(AgentMessageItem {
            id: new_attributed_agent_message_response_item_id().to_string(),
            content: vec![AgentMessageContent::Text {
                text: "Agent message from `agent-id`:\n\nReview this.".to_string(),
            }],
            phase: Some(MessagePhase::Commentary),
            memory_citation: None,
            delivery: None,
            questions: None,
            sub_agent_completion: None,
        }),
        started_at_ms: None,
        completed_at_ms: 0,
    });

    assert_eq!(
        [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated,]
            .map(|mode| should_persist_event_msg(&event, mode)),
        [true, true]
    );
}

#[test]
fn user_shell_completion_is_persisted_in_every_history_mode() {
    let event = EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: codex_protocol::ThreadId::new(),
        turn_id: "shell-turn".to_string(),
        item: TurnItem::CommandExecution(CommandExecutionItem {
            id: "shell-turn".to_string(),
            plugin_id: None,
            script_path: None,
            process_id: Some("12345".to_string()),
            command: vec!["sleep".to_string(), "60".to_string()],
            cwd: test_path_buf("/tmp").abs().into(),
            parsed_cmd: Vec::new(),
            source: ExecCommandSource::UserShell,
            interaction_input: None,
            status: CommandExecutionStatus::Completed,
            stdout: Some(String::new()),
            stderr: Some(String::new()),
            aggregated_output: Some(String::new()),
            exit_code: Some(0),
            duration: Some(Duration::from_secs(1)),
            formatted_output: Some(String::new()),
        }),
        started_at_ms: Some(100),
        completed_at_ms: 200,
    });

    assert_eq!(
        [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated]
            .map(|mode| should_persist_event_msg(&event, mode)),
        [true, true]
    );
}
