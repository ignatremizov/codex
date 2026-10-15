use super::TurnRecoveryMode;
use super::select_recoverable_turn;
use crate::CodexHarnessMetadata;
use crate::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ModeKind;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnStartedEvent;
use pretty_assertions::assert_eq;
use serde_json::json;

fn history(id: ThreadId) -> anyhow::Result<Vec<RolloutItem>> {
    Ok(vec![
        RolloutItem::SessionMeta(Box::new(SessionMetaLine {
            meta: SessionMeta {
                id,
                session_id: id.into(),
                ..Default::default()
            },
            git: None,
        })),
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
        RolloutItem::TurnContext(serde_json::from_value(json!({
            "turn_id": "original",
            "cwd": std::env::current_dir()?,
            "approval_policy": "never",
            "sandbox_policy": { "type": "danger-full-access" },
            "model": "test-model",
            "summary": "auto",
            "recovery_start_options": {}
        }))?),
        RolloutItem::ResponseItem(
            serde_json::from_value::<codex_protocol::models::ResponseItem>(json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "finish"}]
            }))?
            .into(),
        ),
    ])
}

fn abort(reason: TurnAbortReason) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnAborted(TurnAbortedEvent {
        root_turn_id: None,
        turn_id: Some("original".to_string()),
        reason,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    }))
}

#[test]
fn explicit_continuation_only_accepts_interrupted_terminal_state() -> anyhow::Result<()> {
    let id = ThreadId::new();
    for reason in [
        TurnAbortReason::Interrupted,
        TurnAbortReason::Replaced,
        TurnAbortReason::ReviewEnded,
        TurnAbortReason::BudgetLimited,
    ] {
        let mut items = history(id)?;
        let expected = if reason == TurnAbortReason::Interrupted {
            let RolloutItem::TurnContext(context) = &items[2] else {
                anyhow::bail!("missing fixture context");
            };
            Some(context.clone())
        } else {
            None
        };
        items.push(abort(reason));
        assert_eq!(
            select_recoverable_turn(id, &items, TurnRecoveryMode::Explicit)?,
            expected
        );
        assert_eq!(
            select_recoverable_turn(id, &items, TurnRecoveryMode::Automatic)?,
            None
        );
    }
    Ok(())
}

#[test]
fn restarted_turn_can_continue_with_or_without_tool_outcomes() -> anyhow::Result<()> {
    for (call_type, output_type) in [
        ("function_call", "function_call_output"),
        ("custom_tool_call", "custom_tool_call_output"),
        ("local_shell_call", "function_call_output"),
        ("tool_search_call", "tool_search_output"),
    ] {
        let id = ThreadId::new();
        let mut items = history(id)?;
        let arguments = if call_type == "tool_search_call" {
            json!({})
        } else {
            json!("{}")
        };
        items.push(RolloutItem::ResponseItem(
            serde_json::from_value::<codex_protocol::models::ResponseItem>(json!({
                "type": call_type,
                "name": "tool",
                "arguments": arguments,
                "call_id": "call",
                "input": "input",
                "status": "in_progress",
                "execution": "client",
                "action": { "type": "exec", "command": ["example"] }
            }))?
            .into(),
        ));
        items.push(abort(TurnAbortReason::Interrupted));
        items.push(items[1].clone());
        let RolloutItem::TurnContext(expected) = &items[2] else {
            anyhow::bail!("missing fixture context");
        };
        let expected = expected.clone();
        for mode in [TurnRecoveryMode::Automatic, TurnRecoveryMode::Explicit] {
            assert_eq!(
                select_recoverable_turn(id, &items, mode)?,
                Some(expected.clone())
            );
        }
        items.push(RolloutItem::ResponseItem(
            serde_json::from_value::<codex_protocol::models::ResponseItem>(json!({
                "type": output_type,
                "call_id": "call",
                "output": "done",
                "execution": "client",
                "status": "completed",
                "tools": []
            }))?
            .into(),
        ));
        for mode in [TurnRecoveryMode::Automatic, TurnRecoveryMode::Explicit] {
            assert_eq!(
                select_recoverable_turn(id, &items, mode)?,
                Some(expected.clone())
            );
        }
    }
    Ok(())
}

#[test]
fn explicit_selection_does_not_reuse_inherited_input_or_older_turns() -> anyhow::Result<()> {
    let id = ThreadId::new();
    let mut items = history(id)?;
    let RolloutItem::ResponseItem(input) = &mut items[3] else {
        anyhow::bail!("missing fixture input");
    };
    input.metadata = Some(CodexHarnessMetadata {
        inherited_user_message: true,
        ..Default::default()
    });
    assert!(select_recoverable_turn(id, &items, TurnRecoveryMode::Explicit).is_err());
    let mut items = history(id)?;
    items.push(abort(TurnAbortReason::Interrupted));
    let mut newer = items[1].clone();
    if let RolloutItem::EventMsg(EventMsg::TurnStarted(event)) = &mut newer {
        event.turn_id = "newer".to_string();
    }
    items.push(newer);
    assert!(select_recoverable_turn(id, &items, TurnRecoveryMode::Explicit).is_err());
    Ok(())
}

#[test]
fn call_identity_checks_keep_output_families_separate() -> anyhow::Result<()> {
    let id = ThreadId::new();
    let mut items = history(id)?;
    let RolloutItem::TurnContext(expected) = &items[2] else {
        anyhow::bail!("missing fixture context");
    };
    let expected = expected.clone();
    items.push(RolloutItem::ResponseItem(
        serde_json::from_value::<codex_protocol::models::ResponseItem>(json!({
            "type": "tool_search_call",
            "call_id": "search",
            "execution": "client",
            "arguments": {}
        }))?
        .into(),
    ));
    items.push(abort(TurnAbortReason::Interrupted));
    items.push(items[1].clone());
    // Another output family or a provider-owned search cannot clear a pending
    // client call's identity and make a duplicate call unambiguous.
    for output in [
        json!({"type": "function_call_output", "call_id": "search", "output": "done"}),
        json!({
            "type": "tool_search_output",
            "call_id": "search",
            "execution": "server",
            "status": "completed",
            "tools": []
        }),
    ] {
        items.push(RolloutItem::ResponseItem(
            serde_json::from_value::<codex_protocol::models::ResponseItem>(output)?.into(),
        ));
    }
    let duplicate = items[4].clone();
    items.push(duplicate.clone());
    for mode in [TurnRecoveryMode::Automatic, TurnRecoveryMode::Explicit] {
        assert!(select_recoverable_turn(id, &items, mode).is_err());
    }
    items.pop();
    items.push(RolloutItem::ResponseItem(
        serde_json::from_value::<codex_protocol::models::ResponseItem>(json!({
            "type": "tool_search_output",
            "call_id": "search",
            "execution": "client",
            "status": "completed",
            "tools": []
        }))?
        .into(),
    ));
    items.push(duplicate);
    for mode in [TurnRecoveryMode::Automatic, TurnRecoveryMode::Explicit] {
        assert_eq!(
            select_recoverable_turn(id, &items, mode)?,
            Some(expected.clone())
        );
    }
    Ok(())
}
