use codex_protocol::ThreadId;
use codex_protocol::config_types::ModeKind;
use codex_protocol::models::ContentItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::TurnRecoveryStartOptions;
use codex_protocol::protocol::TurnStartedEvent;
use codex_rollout::RolloutItem;
use codex_rollout::TurnRecoveryMode;
use codex_rollout::select_recoverable_turn;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use test_case::test_case;

fn select_unfinished_own_turn(
    thread_id: ThreadId,
    items: &[RolloutItem],
) -> std::io::Result<Option<TurnContextItem>> {
    select_recoverable_turn(thread_id, items, TurnRecoveryMode::Automatic)
}

fn history(thread_id: ThreadId, mode: ThreadHistoryMode) -> anyhow::Result<Vec<RolloutItem>> {
    let cwd = AbsolutePathBuf::try_from(std::env::current_dir()?)?;
    Ok(vec![
        RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                id: thread_id,
                session_id: thread_id.into(),
                history_mode: mode,
                ..Default::default()
            },
            git: None,
        }),
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_attribution: None,
            turn_id: "original".to_string(),
            root_turn_id: None,
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: ModeKind::Default,
            agent_queue: None,
        })),
        RolloutItem::TurnContext(TurnContextItem {
            turn_id: Some("original".to_string()),
            recovery_start_options: Some(TurnRecoveryStartOptions {
                cleared_interrupt_message_ids: Vec::new(),
                final_output_json_schema: None,
                service_tier: None,
                parent_turn_id: None,
                configured_reasoning_summary: None,
                reasoning_summary: Some(codex_protocol::config_types::ReasoningSummary::Auto),
            }),
            root_turn_id: None,
            disabled_plugin_ids: None,
            cwd: cwd.clone(),
            workspace_roots: Some(vec![cwd]),
            current_date: None,
            timezone: None,
            approval_policy: AskForApproval::Never,
            approvals_reviewer: None,
            sandbox_policy: SandboxPolicy::new_read_only_policy(),
            permission_profile: Some(PermissionProfile::read_only()),
            active_permission_profile: None,
            network: None,
            file_system_sandbox_policy: None,
            model: "test-model".to_string(),
            comp_hash: None,
            personality: None,
            collaboration_mode: None,
            multi_agent_version: None,
            multi_agent_mode: None,
            realtime_active: None,
            cyber_access_program: None,
            effort: None,
            summary: Some(codex_protocol::config_types::ReasoningSummary::None),
        }),
        RolloutItem::ResponseItem(
            ResponseItem::Message {
                status: None,
                encrypted_content: None,
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "finish the task".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }
            .into(),
        ),
    ])
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
fn selects_original_unfinished_regular_turn(mode: ThreadHistoryMode) -> anyhow::Result<()> {
    let id = ThreadId::new();
    let items = history(id, mode)?;
    let actual = select_unfinished_own_turn(id, &items)?;
    let RolloutItem::TurnContext(expected) = &items[2] else {
        anyhow::bail!("fixture context missing");
    };
    assert_eq!(
        serde_json::to_value(actual)?,
        serde_json::to_value(Some(expected))?
    );
    Ok(())
}

#[test]
fn completed_and_aborted_turns_are_not_recovered() -> anyhow::Result<()> {
    let id = ThreadId::new();
    for terminal in [
        EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "original".to_string(),
            root_turn_id: None,
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        }),
        EventMsg::TurnAborted(TurnAbortedEvent {
            turn_id: Some("original".to_string()),
            reason: TurnAbortReason::Interrupted,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        }),
    ] {
        let mut items = history(id, ThreadHistoryMode::Legacy)?;
        items.push(RolloutItem::EventMsg(terminal));
        assert!(select_unfinished_own_turn(id, &items)?.is_none());
    }
    Ok(())
}

#[test]
fn inherited_ancestor_turn_cannot_wake_idle_child() -> anyhow::Result<()> {
    let id = ThreadId::new();
    let mut items = history(id, ThreadHistoryMode::Paginated)?;
    let boundary = items.len() as u64;
    let RolloutItem::SessionMeta(meta) = &mut items[0] else {
        anyhow::bail!("fixture metadata missing");
    };
    meta.meta.parent_thread_id = Some(ThreadId::new());
    meta.meta.subagent_history_start_ordinal = Some(boundary);
    assert!(select_unfinished_own_turn(id, &items)?.is_none());
    Ok(())
}

#[test]
fn foreign_or_unbounded_inherited_history_is_rejected() -> anyhow::Result<()> {
    let id = ThreadId::new();
    let mut items = history(id, ThreadHistoryMode::Legacy)?;
    assert!(select_unfinished_own_turn(ThreadId::new(), &items).is_err());
    let RolloutItem::SessionMeta(meta) = &mut items[0] else {
        anyhow::bail!("fixture metadata missing");
    };
    meta.meta.parent_thread_id = Some(ThreadId::new());
    assert!(select_unfinished_own_turn(id, &items).is_err());
    Ok(())
}

#[test]
fn newer_turn_without_trustworthy_context_supersedes_old_work() -> anyhow::Result<()> {
    let id = ThreadId::new();
    let mut items = history(id, ThreadHistoryMode::Legacy)?;
    let mut newer = items[1].clone();
    if let RolloutItem::EventMsg(EventMsg::TurnStarted(start)) = &mut newer {
        start.turn_id = "newer".to_string();
    }
    items.push(newer);
    assert!(select_unfinished_own_turn(id, &items).is_err());
    Ok(())
}

#[test]
fn repeated_recovery_start_keeps_original_input_and_identity() -> anyhow::Result<()> {
    let id = ThreadId::new();
    let mut items = history(id, ThreadHistoryMode::Legacy)?;
    items.extend([items[1].clone(), items[2].clone()]);
    let context = select_unfinished_own_turn(id, &items)?
        .ok_or_else(|| anyhow::anyhow!("unfinished recovery lost its original input"))?;
    assert_eq!(context.turn_id, Some("original".to_string()));
    Ok(())
}

#[test]
fn durable_received_mail_can_resume_but_outbound_mail_cannot() -> anyhow::Result<()> {
    let id = ThreadId::new();
    let mut items = history(id, ThreadHistoryMode::Legacy)?;
    items.pop();
    assert!(select_unfinished_own_turn(id, &items).is_err());
    let child = codex_protocol::AgentPath::from_string("/root/worker".to_string())
        .map_err(anyhow::Error::msg)?;
    items.push(RolloutItem::InterAgentCommunication(
        codex_protocol::protocol::InterAgentCommunication::new(
            child.clone(),
            codex_protocol::AgentPath::root(),
            Vec::new(),
            "Follow up on the result".to_string(),
            /*trigger_turn*/ true,
        ),
    ));
    assert!(select_unfinished_own_turn(id, &items)?.is_some());
    let Some(RolloutItem::InterAgentCommunication(message)) = items.last_mut() else {
        anyhow::bail!("fixture mail missing");
    };
    message.recipient = child;
    assert!(select_unfinished_own_turn(id, &items).is_err());
    Ok(())
}

#[test]
fn old_context_without_recorded_start_options_requires_explicit_retry() -> anyhow::Result<()> {
    let id = ThreadId::new();
    let mut items = history(id, ThreadHistoryMode::Legacy)?;
    let RolloutItem::TurnContext(context) = &mut items[2] else {
        anyhow::bail!("fixture context missing");
    };
    context.recovery_start_options = None;
    assert!(
        select_unfinished_own_turn(id, &items)
            .err()
            .is_some_and(|err| err.to_string().contains("start options were not recorded"))
    );
    Ok(())
}

#[test_case("function_call", "function_call_output"; "function")]
#[test_case("custom_tool_call", "custom_tool_call_output"; "custom tool")]
#[test_case("local_shell_call", "function_call_output"; "local shell")]
fn unfinished_tool_calls_preserve_original_recovery_context(
    call_type: &str,
    output_type: &str,
) -> anyhow::Result<()> {
    let id = ThreadId::new();
    let mut items = history(id, ThreadHistoryMode::Paginated)?;
    let RolloutItem::TurnContext(expected) = &items[2] else {
        anyhow::bail!("fixture context missing");
    };
    let expected = expected.clone();
    let call = serde_json::from_value::<ResponseItem>(serde_json::json!({
        "type": call_type,
        "call_id": "operation",
        "name": "tool",
        "arguments": "{}",
        "input": "input",
        "status": "in_progress",
        "action": { "type": "exec", "command": ["example"] },
    }))?;
    items.push(RolloutItem::ResponseItem(call.into()));
    assert_eq!(
        select_unfinished_own_turn(id, &items)?,
        Some(expected.clone())
    );
    items.push(RolloutItem::ResponseItem(
        serde_json::from_value::<ResponseItem>(serde_json::json!({
            "type": output_type, "call_id": "operation", "output": "done",
        }))?
        .into(),
    ));
    assert_eq!(select_unfinished_own_turn(id, &items)?, Some(expected));
    Ok(())
}
