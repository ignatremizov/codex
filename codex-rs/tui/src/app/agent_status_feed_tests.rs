use super::*;
use codex_app_server_protocol::CommandExecutionSource;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;

#[test]
fn source_control_audit_is_not_reported_as_the_agents_own_progress() {
    let item = ThreadItemPayload::from(codex_protocol::items::TurnItem::UserAgentControl(
        codex_protocol::items::UserAgentControlItem::succeeded(
            codex_protocol::items::UserAgentControlAction::Prompt,
        ),
    ));
    assert_eq!(activity_summary(&item), None);
}

#[test]
fn batch_status_preview_does_not_claim_live_delivery() {
    let item = ThreadItemPayload::CollabAgentToolCall {
        lifecycle: None,
        id: "batch".into(),
        tool: CollabAgentTool::SendInput,
        status: codex_app_server_protocol::CollabAgentToolCallStatus::Completed,
        observe_commentary: Some(false),
        wake_on_completion: None,
        target_messages: Some(false),
        queue_input: Some(false),
        mailbox_input: Some(true),
        input_batch: Some(codex_protocol::CollabAgentInputBatch {
            flags: "z".into(),
            sender_thread_id: None,
            results: vec![codex_protocol::CollabAgentInputResult {
                target: "43".into(),
                receiver_thread_id: None,
                status: codex_protocol::CollabAgentInputStatus::MailboxAccepted,
                error: None,
                hint: None,
            }],
        }),
        sender_thread_id: "sender".into(),
        receiver_thread_ids: Vec::new(),
        receiver_agents: Vec::new(),
        prompt: Some("Private input.".into()),
        model: None,
        reasoning_effort: None,
        agents_states: Default::default(),
    };
    insta::assert_snapshot!(activity_summary(&item).unwrap(), @"Input batch: 1 receiver results");
}

#[test]
fn ownership_status_preview_distinguishes_actions_and_outcomes() {
    use codex_app_server_protocol::CollabAgentLifecycle;
    use codex_app_server_protocol::CollabAgentLifecycleAction;
    use codex_app_server_protocol::CollabAgentToolCallStatus;

    let mut summaries = Vec::new();
    for action in [
        CollabAgentLifecycleAction::Detach,
        CollabAgentLifecycleAction::Reattach,
    ] {
        for status in [
            CollabAgentToolCallStatus::InProgress,
            CollabAgentToolCallStatus::Completed,
            CollabAgentToolCallStatus::Failed,
            CollabAgentToolCallStatus::Interrupted,
        ] {
            let item = ThreadItemPayload::CollabAgentToolCall {
                lifecycle: Some(CollabAgentLifecycle {
                    action,
                    root_thread_id: "root".into(),
                }),
                id: "ownership".into(),
                tool: match action {
                    CollabAgentLifecycleAction::Detach => CollabAgentTool::CloseAgent,
                    CollabAgentLifecycleAction::Reattach => CollabAgentTool::ResumeAgent,
                },
                status,
                observe_commentary: None,
                wake_on_completion: None,
                target_messages: None,
                queue_input: None,
                mailbox_input: None,
                input_batch: None,
                sender_thread_id: "root".into(),
                receiver_thread_ids: vec!["child".into()],
                receiver_agents: Vec::new(),
                prompt: None,
                model: None,
                reasoning_effort: None,
                agents_states: Default::default(),
            };
            summaries.push(activity_summary(&item).expect("ownership summary"));
        }
    }
    insta::assert_snapshot!(summaries.join("\n"), @r"
    Detaching an agent
    Detached an agent
    Failed to detach an agent
    Detachment interrupted
    Reattaching an agent
    Reattached an agent
    Failed to reattach an agent
    Reattachment interrupted
    ");
}

#[test]
fn agent_status_uses_bounded_buffered_activity() {
    let mut store = ThreadEventStore::new(/*capacity*/ 8);
    store.push_notification(ServerNotification::ItemCompleted(
        ItemCompletedNotification::new(
            "thread-child".to_string(),
            "turn-1".to_string(),
            ThreadItemPayload::CommandExecution {
                model_context: None,
                sandbox_type: None,
                id: "command-1".to_string(),
                command: "cargo test -p codex-tui".to_string(),
                cwd: AbsolutePathBuf::try_from("/workspace")
                    .expect("absolute path")
                    .into(),
                process_id: None,
                plugin_id: None,
                script_path: None,
                source: CommandExecutionSource::Agent,
                user_shell_response_handling: None,
                status: CommandExecutionStatus::Completed,
                command_actions: Vec::new(),
                aggregated_output: Some("unbounded output\n".repeat(10_000)),
                exit_code: Some(0),
                duration_ms: Some(42),
            },
            /*started_at_ms*/ None,
            /*completed_at_ms*/ 1,
        ),
    ));
    store.push_notification(ServerNotification::ItemCompleted(
        ItemCompletedNotification::new(
            "thread-child".to_string(),
            "turn-1".to_string(),
            ThreadItemPayload::AgentMessage {
                inter_agent_source: None,
                id: "message-1".to_string(),
                text: "Finished checking the focused TUI tests.".to_string(),
                phase: None,
                memory_citation: None,
                attribution: None,
                input: None,
                delivery: None,
                questions: None,
            },
            /*started_at_ms*/ None,
            /*completed_at_ms*/ 2,
        ),
    ));

    store.push_notification(ServerNotification::ItemCompleted(
        ItemCompletedNotification::new(
            "thread-child".to_string(),
            "turn-1".to_string(),
            ThreadItemPayload::AgentMessage {
                id: "incoming-task".to_string(),
                attribution: None,
                input: None,
                text: "Incoming task must not replace the agent's own activity.".to_string(),
                inter_agent_source: Some(codex_app_server_protocol::InterAgentMessageSource {
                    author: "/root".to_string(),
                    recipient: "/root/reviewer".to_string(),
                }),
                phase: Some(codex_protocol::models::MessagePhase::FinalAnswer),
                memory_citation: None,
                delivery: None,
                questions: None,
            },
            /*started_at_ms*/ None,
            /*completed_at_ms*/ 3,
        ),
    ));
    let preview = AgentStatusThreadPreview::from_store("/root/reviewer".to_string(), &store);
    let cell = AgentStatusHistoryCell::new(vec![preview]);
    let rendered = cell
        .display_lines(/*width*/ 80)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(rendered, @r###"
    /subagents
    Sub-agents running

      • `/root/reviewer`
        $ cargo test -p codex-tui
        Finished checking the focused TUI tests.
    "###);
    assert!(!rendered.contains("unbounded output"));
}

#[test]
fn agent_status_uses_reasoning_summaries_only() {
    let mut store = ThreadEventStore::new(/*capacity*/ 8);
    store.push_notification(ServerNotification::ItemCompleted(
        ItemCompletedNotification::new(
            "thread-child".to_string(),
            "turn-1".to_string(),
            ThreadItemPayload::Reasoning {
                id: "reasoning-with-summary".to_string(),
                summary: vec!["safe summary".to_string()],
                content: vec!["hidden raw reasoning".to_string()],
            },
            /*started_at_ms*/ None,
            /*completed_at_ms*/ 1,
        ),
    ));
    store.push_notification(ServerNotification::ItemCompleted(
        ItemCompletedNotification::new(
            "thread-child".to_string(),
            "turn-1".to_string(),
            ThreadItemPayload::Reasoning {
                id: "reasoning-without-summary".to_string(),
                summary: Vec::new(),
                content: vec!["raw-only reasoning".to_string()],
            },
            /*started_at_ms*/ None,
            /*completed_at_ms*/ 2,
        ),
    ));

    let preview = AgentStatusThreadPreview::from_store("/root/reviewer".to_string(), &store);
    let cell = AgentStatusHistoryCell::new(vec![preview]);
    let rendered = cell
        .display_lines(/*width*/ 80)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(rendered, @r###"
    /subagents
    Sub-agents running

      • `/root/reviewer`
        safe summary
    "###);
    assert!(!rendered.contains("hidden raw reasoning"));
    assert!(!rendered.contains("raw-only reasoning"));
}
