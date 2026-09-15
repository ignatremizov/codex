use super::*;
use crate::exec_cell::OutputPreviewLineLimits;
use crate::history_cell::HistoryCell;
use crate::test_support::PathBufExt;
use crate::test_support::test_path_buf;
use crate::thread_transcript::RawReasoningVisibility;
use crate::thread_transcript::join_exploration_groups;
use crate::thread_transcript::thread_items_to_transcript_cells;
use crate::thread_transcript::thread_items_to_transcript_cells_with_preview_line_limits;
use codex_app_server_protocol::CommandAction;
use codex_app_server_protocol::McpToolCallResult;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStatus;
use codex_utils_path_uri::LegacyAppPathString;
use pretty_assertions::assert_eq;
use serde_json::json;

fn command_item(status: CommandExecutionStatus) -> ThreadItem {
    ThreadItem::CommandExecution {
        id: "command".to_string(),
        plugin_id: None,
        script_path: None,
        model_context: None,
        command: "cargo check".to_string(),
        cwd: LegacyAppPathString::from_string("/tmp/project"),
        process_id: None,
        source: CommandExecutionSource::Agent,
        user_shell_response_handling: None,
        status,
        command_actions: vec![CommandAction::Unknown {
            command: "cargo check".to_string(),
        }],
        aggregated_output: Some(
            (1..=12)
                .map(|line| format!("output line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        exit_code: Some(0),
        duration_ms: Some(-5),
    }
}

fn mcp_item(server: &str, id: &str) -> ThreadItem {
    ThreadItem::McpToolCall {
        id: id.to_string(),
        server: server.to_string(),
        tool: "js".to_string(),
        status: McpToolCallStatus::Completed,
        arguments: json!({"title": format!("Inspect page {id}"), "code": "await cua.getState()"}),
        app_context: None,
        mcp_app_resource_uri: None,
        plugin_id: None,
        read_only_hint: None,
        mcp_app_ui: None,
        result: Some(Box::new(McpToolCallResult {
            content: vec![json!({"type": "text", "text": format!("Full result for {id}")})],
            structured_content: None,
            meta: None,
        })),
        error: None,
        duration_ms: Some(5),
    }
}

#[test]
fn completed_tools_keep_compact_and_detailed_presentations() {
    let cwd = test_path_buf("/workspace").abs();
    let items = [
        command_item(CommandExecutionStatus::Completed),
        mcp_item("example", "mcp"),
    ];
    let cells = thread_items_to_transcript_cells(
        /*thread_id*/ None,
        &cwd,
        items,
        RawReasoningVisibility::Hidden,
        /*config*/ None,
    );
    assert_eq!(cells.len(), 2);
    let rendered = ["Command", "MCP"]
        .into_iter()
        .zip(cells)
        .map(|(label, cell)| {
            assert_eq!(cell.transcript_animation_tick(), None);
            let display = cell
                .display_lines(/*width*/ 80)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            let transcript = cell
                .transcript_lines(/*width*/ 80)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            format!("{label}: compact\n{display}\n\n{label}: detailed\n{transcript}")
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    insta::assert_snapshot!("completed_tool_presentations", rendered);
}

#[test]
fn replayed_commands_keep_explicit_preview_limits_including_zero() {
    let cwd = test_path_buf("/workspace").abs();
    for (limits, expected) in [
        (
            OutputPreviewLineLimits {
                command: 2,
                user_shell: 4,
            },
            vec![
                "• Ran cargo check",
                "  └ output line 1",
                "    … +11 rows (ctrl+t to view transcript)",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>(),
        ),
        (
            OutputPreviewLineLimits {
                command: 0,
                user_shell: 0,
            },
            std::iter::once("• Ran cargo check".to_owned())
                .chain((1..=12).map(|line| {
                    if line == 1 {
                        format!("  └ output line {line}")
                    } else {
                        format!("    output line {line}")
                    }
                }))
                .collect::<Vec<_>>(),
        ),
    ] {
        let cells = thread_items_to_transcript_cells_with_preview_line_limits(
            /*thread_id*/ None,
            &cwd,
            [command_item(CommandExecutionStatus::Completed)],
            RawReasoningVisibility::Hidden,
            /*config*/ None,
            limits,
            crate::multi_agents::AgentPreviewLineLimits::default(),
        );
        let cell = cells[0].as_any().downcast_ref::<ExecCell>().unwrap();
        assert_eq!(cell.output_preview_line_limits(), limits);
        assert_eq!(
            cell.display_lines(/*width*/ 80)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            expected,
        );
        assert_eq!(
            cell.compact_hyperlink_lines(/*width*/ 80),
            cell.display_hyperlink_lines(/*width*/ 80),
        );
        assert_eq!(
            cell.transcript_lines(/*width*/ 80)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            std::iter::once("$ cargo check".to_owned())
                .chain((1..=12).map(|line| format!("output line {line}")))
                .chain(std::iter::once("✓ • 0ms".to_owned()))
                .collect::<Vec<_>>(),
        );
    }
}

#[test]
fn split_exploration_replay_join_preserves_preview_rendering() {
    let cwd = test_path_buf("/workspace").abs();
    let limits = OutputPreviewLineLimits {
        command: 2,
        user_shell: 4,
    };
    let items = ["older", "newer"]
        .into_iter()
        .map(|id| {
            let mut item = command_item(CommandExecutionStatus::Completed);
            if let ThreadItem::CommandExecution {
                id: item_id,
                command,
                command_actions,
                ..
            } = &mut item
            {
                *item_id = id.to_owned();
                *command = format!("cat {id}.rs");
                *command_actions = vec![CommandAction::Read {
                    command: command.clone(),
                    name: format!("{id}.rs"),
                    path: LegacyAppPathString::from_string(format!("/workspace/{id}.rs")),
                }];
            }
            item
        })
        .collect::<Vec<_>>();
    let turn = codex_app_server_protocol::Turn {
        id: "turn".to_owned(),
        items: items.clone(),
        items_view: TurnItemsView::Full,
        status: TurnStatus::Completed,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    };
    let project = |items: Vec<ThreadItem>| {
        thread_items_to_transcript_cells_with_preview_line_limits(
            /*thread_id*/ None,
            &cwd,
            items,
            RawReasoningVisibility::Hidden,
            /*config*/ None,
            limits,
            crate::multi_agents::AgentPreviewLineLimits::default(),
        )
    };
    let older = project(vec![items[0].clone()]).remove(/*index*/ 0);
    let newer = project(vec![items[1].clone()]).remove(/*index*/ 0);
    let joined = join_exploration_groups(&older, &newer, std::slice::from_ref(&turn))
        .expect("split exploration groups should join");
    let expected = project(items).remove(/*index*/ 0);
    assert_eq!(
        joined
            .as_any()
            .downcast_ref::<ExecCell>()
            .unwrap()
            .output_preview_line_limits(),
        limits
    );
    assert_eq!(
        joined.display_lines(/*width*/ 80),
        expected.display_lines(/*width*/ 80),
    );
    assert_eq!(
        joined.compact_hyperlink_lines(/*width*/ 80),
        expected.compact_hyperlink_lines(/*width*/ 80),
    );
    assert_eq!(
        joined.transcript_lines(/*width*/ 80),
        expected.transcript_lines(/*width*/ 80),
    );
}

#[test]
fn failed_commands_keep_failure_when_exit_code_is_missing_or_zero() {
    for exit in [None, Some(0)] {
        let mut item = command_item(CommandExecutionStatus::Failed);
        if let ThreadItem::CommandExecution { exit_code, .. } = &mut item {
            *exit_code = exit;
        }
        let command = CommandHistory::from_item(item).unwrap();
        assert_eq!((command.exit_code, command.duration), (1, Duration::ZERO));
    }
}

#[test]
fn incomplete_tool_payloads_preserve_last_known_status_and_output() {
    let command = command_item(CommandExecutionStatus::InProgress);
    let mut mcp = mcp_item("example", "pending");
    if let ThreadItem::McpToolCall { status, result, .. } = &mut mcp {
        *status = McpToolCallStatus::InProgress;
        *result = None;
    }
    let mut missing_result = mcp_item("example", "completed");
    if let ThreadItem::McpToolCall { result, .. } = &mut missing_result {
        *result = None;
    }
    let cwd = test_path_buf("/workspace").abs();
    let rendered = thread_items_to_transcript_cells(
        /*thread_id*/ None,
        &cwd,
        [command, mcp, missing_result],
        RawReasoningVisibility::Hidden,
        /*config*/ None,
    )
    .into_iter()
    .flat_map(|cell| cell.display_lines(/*width*/ 80))
    .map(|line| line.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    insta::assert_snapshot!("pending_tool_presentations", rendered);
}

#[test]
fn historical_command_fallbacks_preserve_status_output_and_group_boundaries() {
    let cwd = test_path_buf("/workspace").abs();
    let exploration = |call_id: &str| {
        let mut item = command_item(CommandExecutionStatus::Completed);
        if let ThreadItem::CommandExecution {
            id,
            command,
            command_actions,
            ..
        } = &mut item
        {
            *id = call_id.to_owned();
            *command = "cat README.md".to_owned();
            *command_actions = vec![CommandAction::Read {
                command: command.clone(),
                name: "README.md".to_owned(),
                path: LegacyAppPathString::from_string("/workspace/README.md"),
            }];
        }
        item
    };
    let mut snapshots = Vec::new();
    for (source, status, recorded_exit, label) in [
        (
            CommandExecutionSource::Agent,
            CommandExecutionStatus::Declined,
            None,
            "declined",
        ),
        (
            CommandExecutionSource::Agent,
            CommandExecutionStatus::Completed,
            Some(0),
            "opaque command",
        ),
        (
            CommandExecutionSource::Agent,
            CommandExecutionStatus::Completed,
            Some(0),
            "UNC command",
        ),
        (
            CommandExecutionSource::UnifiedExecInteraction,
            CommandExecutionStatus::Completed,
            Some(0),
            "completed interaction",
        ),
        (
            CommandExecutionSource::UnifiedExecInteraction,
            CommandExecutionStatus::Failed,
            Some(7),
            "failed interaction",
        ),
        (
            CommandExecutionSource::UnifiedExecInteraction,
            CommandExecutionStatus::Failed,
            None,
            "failed interaction without exit code",
        ),
        (
            CommandExecutionSource::UnifiedExecInteraction,
            CommandExecutionStatus::InProgress,
            None,
            "pending interaction",
        ),
    ] {
        let mut item = command_item(status);
        if let ThreadItem::CommandExecution {
            source: item_source,
            command,
            aggregated_output,
            exit_code,
            ..
        } = &mut item
        {
            *item_source = source;
            if label == "opaque command" {
                *command = r#"C:\Program Files\Git\bin\bash.exe -lc "echo hi""#.to_owned();
            } else if label == "UNC command" {
                *command = r"\\server\share\tool.exe -arg".to_owned();
            }
            *aggregated_output = Some("first line\n  indented output\nlast line".to_owned());
            *exit_code = recorded_exit;
        }
        let cells = thread_items_to_transcript_cells(
            /*thread_id*/ None,
            &cwd,
            [exploration("before"), item, exploration("after")],
            RawReasoningVisibility::Hidden,
            /*config*/ None,
        );
        assert_eq!(cells.len(), 3);
        for cell in [&cells[0], &cells[2]] {
            assert!(
                cell.as_any()
                    .downcast_ref::<ExecCell>()
                    .unwrap()
                    .is_exploring_cell()
            );
        }
        let [compact, detailed, raw] = [
            cells[1].display_lines(/*width*/ 80),
            cells[1].transcript_lines(/*width*/ 80),
            cells[1].raw_lines(),
        ]
        .map(|lines| {
            lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        });
        assert_eq!((&detailed, &raw), (&compact, &compact));
        snapshots.push(format!("{label}\n{compact}"));
    }
    insta::assert_snapshot!(snapshots.join("\n\n"));
}

#[test]
fn agent_tool_fallbacks_preserve_status_without_duplicating_v2_activity() {
    let cwd = test_path_buf("/workspace").abs();
    let mut snapshots = Vec::new();
    for (tool, status) in [
        (
            CollabAgentTool::SpawnAgent,
            CollabAgentToolCallStatus::InProgress,
        ),
        (
            CollabAgentTool::SendInput,
            CollabAgentToolCallStatus::Failed,
        ),
        (
            CollabAgentTool::CloseAgent,
            CollabAgentToolCallStatus::Interrupted,
        ),
        (
            CollabAgentTool::ResumeAgent,
            CollabAgentToolCallStatus::Failed,
        ),
        (
            CollabAgentTool::Wait,
            CollabAgentToolCallStatus::Interrupted,
        ),
        (
            CollabAgentTool::SendMessage,
            CollabAgentToolCallStatus::InProgress,
        ),
        (
            CollabAgentTool::FollowupTask,
            CollabAgentToolCallStatus::InProgress,
        ),
        (
            CollabAgentTool::InterruptAgent,
            CollabAgentToolCallStatus::InProgress,
        ),
        (
            CollabAgentTool::ListAgents,
            CollabAgentToolCallStatus::InProgress,
        ),
    ] {
        let visible = matches!(
            tool,
            CollabAgentTool::SpawnAgent
                | CollabAgentTool::SendInput
                | CollabAgentTool::CloseAgent
                | CollabAgentTool::ResumeAgent
                | CollabAgentTool::Wait
        );
        let item = ThreadItem::CollabAgentToolCall {
            id: "pending-agent-call".to_string(),
            tool,
            status,
            observe_commentary: None,
            wake_on_completion: None,
            target_messages: None,
            queue_input: None,
            input_batch: None,
            mailbox_input: None,
            sender_thread_id: "00000000-0000-0000-0000-000000000001".to_string(),
            receiver_thread_ids: vec!["00000000-0000-0000-0000-000000000002".to_string()],
            receiver_agents: Vec::new(),
            prompt: Some("Inspect the parser".to_string()),
            model: None,
            reasoning_effort: None,
            agents_states: Default::default(),
        };
        let cells = thread_items_to_transcript_cells(
            /*thread_id*/ None,
            &cwd,
            [item],
            RawReasoningVisibility::Hidden,
            /*config*/ None,
        );
        assert_eq!(cells.len(), usize::from(visible));
        for cell in cells {
            assert_eq!(cell.transcript_animation_tick(), None);
            let text = cell
                .transcript_lines(/*width*/ 80)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            snapshots.push(text);
        }
    }
    insta::assert_snapshot!(snapshots.join("\n"));
}
