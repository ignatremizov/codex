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
        RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                id,
                session_id: id.into(),
                ..Default::default()
            },
            git: None,
        }),
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
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
fn restarted_turn_uses_latest_lifecycle_but_retains_all_calls() -> anyhow::Result<()> {
    let id = ThreadId::new();
    let mut items = history(id)?;
    items.push(RolloutItem::ResponseItem(
        serde_json::from_value::<codex_protocol::models::ResponseItem>(json!({
            "type": "function_call", "name": "tool", "arguments": "{}", "call_id": "call"
        }))?
        .into(),
    ));
    items.push(abort(TurnAbortReason::Interrupted));
    items.push(items[1].clone());
    for mode in [TurnRecoveryMode::Automatic, TurnRecoveryMode::Explicit] {
        assert!(select_recoverable_turn(id, &items, mode).is_err());
    }
    items.push(RolloutItem::ResponseItem(
        serde_json::from_value::<codex_protocol::models::ResponseItem>(json!({
            "type": "function_call_output", "call_id": "call", "output": "done"
        }))?
        .into(),
    ));
    let RolloutItem::TurnContext(expected) = &items[2] else {
        anyhow::bail!("missing fixture context");
    };
    for mode in [TurnRecoveryMode::Automatic, TurnRecoveryMode::Explicit] {
        assert_eq!(
            select_recoverable_turn(id, &items, mode)?,
            Some(expected.clone())
        );
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
fn client_tool_search_requires_its_own_durable_output_across_restart() -> anyhow::Result<()> {
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
    // Neither another tool family's result nor a provider-owned search proves
    // completion of the client call. Prompt normalization must not fill this gap.
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
        for mode in [TurnRecoveryMode::Automatic, TurnRecoveryMode::Explicit] {
            assert!(select_recoverable_turn(id, &items, mode).is_err());
        }
    }
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
    for mode in [TurnRecoveryMode::Automatic, TurnRecoveryMode::Explicit] {
        assert_eq!(
            select_recoverable_turn(id, &items, mode)?,
            Some(expected.clone())
        );
    }
    Ok(())
}
