use super::*;
use crate::protocol::thread_history::build_turns_from_rollout_items;
use crate::protocol::v2::Turn;
use crate::protocol::v2::TurnItemsView;
use crate::protocol::v2::TurnStatus;
use codex_history::RolloutItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_utils_absolute_path::test_support::PathBufExt;
use pretty_assertions::assert_eq;
use std::time::Duration;

fn turn_metadata(turn_id: &str) -> Option<InternalChatMessageMetadataPassthrough> {
    Some(InternalChatMessageMetadataPassthrough {
        turn_id: Some(turn_id.to_string()),
        create_time: None,
        content_item_kinds: None,
        executed_tool_calls: None,
    })
}

fn function_call(turn_id: &str, name: &str, call_id: &str, arguments: &str) -> RolloutItem {
    RolloutItem::ResponseItem(
        ResponseItem::FunctionCall {
            id: None,
            name: name.to_string(),
            namespace: None,
            arguments: arguments.to_string(),
            encrypted_function_args: None,
            call_id: call_id.to_string(),
            internal_chat_message_metadata_passthrough: turn_metadata(turn_id),
        }
        .into(),
    )
}

fn function_call_output(turn_id: &str, call_id: &str, output: &str) -> RolloutItem {
    RolloutItem::ResponseItem(
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some(call_id.to_string()),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload::from_text(output.to_string()),
            internal_chat_message_metadata_passthrough: turn_metadata(turn_id),
        }
        .into(),
    )
}

fn turn_started(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_id.to_string(),
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
        agent_queue: None,
    }))
}

fn turn_complete(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: turn_id.to_string(),
        last_agent_message: None,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }))
}

#[test]
fn non_paginated_exec_call_and_output_rebuild_completed_command_item() {
    let turn_id = "turn-1";
    let items = vec![
        turn_started(turn_id),
        function_call(
            turn_id,
            "exec_command",
            "exec-1",
            r#"{"cmd":"sleep 20","workdir":"/home/user/project"}"#,
        ),
        function_call_output(
            turn_id,
            "exec-1",
            "Chunk ID: chunk-1\nWall time: 19.8895 seconds\nProcess exited with code 0\nOriginal token count: 0\nOutput:\n",
        ),
        turn_complete(turn_id),
    ];

    assert_eq!(
        build_turns_from_rollout_items(&items),
        vec![Turn {
            id: turn_id.to_string(),
            items: vec![ThreadItem::CommandExecution {
                id: "exec-1".to_string(),
                plugin_id: None,
                script_path: None,
                command: "sleep 20".to_string(),
                cwd: LegacyAppPathString::from_string("/home/user/project"),
                process_id: None,
                source: CommandExecutionSource::UnifiedExecStartup,
                status: CommandExecutionStatus::Completed,
                command_actions: vec![CommandAction::Unknown {
                    command: "sleep 20".to_string(),
                }],
                aggregated_output: None,
                exit_code: Some(0),
                duration_ms: Some(19_889),
            }],
            items_view: TurnItemsView::Full,
            error: None,
            status: TurnStatus::Completed,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        }]
    );
}

#[test]
fn non_paginated_write_stdin_output_updates_original_command_item() {
    let turn_id = "turn-1";
    let items = vec![
        turn_started(turn_id),
        function_call(
            turn_id,
            "exec_command",
            "exec-1",
            r#"{"cmd":"printf first; sleep 1; printf second","workdir":"/tmp"}"#,
        ),
        function_call_output(
            turn_id,
            "exec-1",
            "Chunk ID: chunk-1\nWall time: 0.5000 seconds\nProcess running with session ID 42\nOutput:\nfirst",
        ),
        function_call(
            turn_id,
            "write_stdin",
            "poll-1",
            r#"{"session_id":42,"chars":"","yield_time_ms":5000}"#,
        ),
        function_call_output(
            turn_id,
            "poll-1",
            "Chunk ID: chunk-2\nWall time: 0.5000 seconds\nProcess exited with code 0\nOutput:\nsecond",
        ),
        turn_complete(turn_id),
    ];

    let turns = build_turns_from_rollout_items(&items);
    assert_eq!(
        turns[0].items,
        vec![ThreadItem::CommandExecution {
            id: "exec-1".to_string(),
            plugin_id: None,
            script_path: None,
            command: "printf first; sleep 1; printf second".to_string(),
            cwd: LegacyAppPathString::from_string("/tmp"),
            process_id: Some("42".to_string()),
            source: CommandExecutionSource::UnifiedExecStartup,
            status: CommandExecutionStatus::Completed,
            command_actions: vec![CommandAction::Unknown {
                command: "printf first; sleep 1; printf second".to_string(),
            }],
            aggregated_output: Some("firstsecond".to_string()),
            exit_code: Some(0),
            duration_ms: Some(1_000),
        }]
    );
}

#[test]
fn canonical_paginated_item_replaces_reconstructed_non_paginated_item() {
    let turn_id = "turn-1";
    let command_item = codex_protocol::items::TurnItem::CommandExecution(
        codex_protocol::items::CommandExecutionItem {
            id: "exec-1".to_string(),
            plugin_id: Some("plugin@example".to_string()),
            script_path: Some("scripts/run.sh".to_string()),
            process_id: None,
            command: vec![
                "bash".to_string(),
                "-lc".to_string(),
                "echo canonical".to_string(),
            ],
            cwd: codex_utils_absolute_path::test_support::test_path_buf("/tmp")
                .abs()
                .into(),
            parsed_cmd: vec![codex_protocol::parse_command::ParsedCommand::Unknown {
                cmd: "echo canonical".to_string(),
            }],
            source: codex_protocol::protocol::ExecCommandSource::Agent,
            interaction_input: None,
            status: codex_protocol::items::CommandExecutionStatus::Completed,
            stdout: Some("canonical\n".to_string()),
            stderr: Some(String::new()),
            aggregated_output: Some("canonical\n".to_string()),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(2)),
            formatted_output: Some("canonical\n".to_string()),
        },
    );
    let items = vec![
        turn_started(turn_id),
        function_call(
            turn_id,
            "exec_command",
            "exec-1",
            r#"{"cmd":"echo raw","workdir":"/tmp"}"#,
        ),
        function_call_output(
            turn_id,
            "exec-1",
            "Chunk ID: chunk-1\nWall time: 0.0010 seconds\nProcess exited with code 0\nOutput:\nraw\n",
        ),
        RolloutItem::EventMsg(EventMsg::ItemCompleted(
            codex_protocol::protocol::ItemCompletedEvent {
                thread_id: codex_protocol::ThreadId::new(),
                turn_id: turn_id.to_string(),
                item: command_item,
                started_at_ms: Some(0),
                completed_at_ms: 2,
            },
        )),
        turn_complete(turn_id),
    ];

    let turns = build_turns_from_rollout_items(&items);
    assert_eq!(
        turns[0].items,
        vec![ThreadItem::CommandExecution {
            id: "exec-1".to_string(),
            plugin_id: Some("plugin@example".to_string()),
            script_path: Some("scripts/run.sh".to_string()),
            command: "bash -lc 'echo canonical'".to_string(),
            cwd: LegacyAppPathString::from_string("/tmp"),
            process_id: None,
            source: CommandExecutionSource::Agent,
            status: CommandExecutionStatus::Completed,
            command_actions: vec![CommandAction::Unknown {
                command: "echo canonical".to_string(),
            }],
            aggregated_output: Some("canonical\n".to_string()),
            exit_code: Some(0),
            duration_ms: Some(2),
        }]
    );
}
