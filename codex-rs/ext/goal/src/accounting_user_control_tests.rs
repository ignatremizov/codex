use super::*;
use codex_protocol::items::UserAgentControlAction;
use codex_protocol::items::UserAgentControlItem;
use pretty_assertions::assert_eq;

#[test]
fn source_control_audit_is_not_model_progress_or_a_final_answer() {
    let state = GoalAccountingState::default();
    state.start_turn("source", ModeKind::Default, &TokenUsage::default());
    state.inner().consecutive_empty_turns = 2;
    state.record_item(
        "source",
        &TurnItem::UserAgentControl(UserAgentControlItem::succeeded(
            UserAgentControlAction::Spawn,
        )),
    );
    let inner = state.inner();
    let turn = inner.turns.get("source").expect("source turn");
    assert_eq!(
        (
            turn.has_activity,
            turn.empty_final,
            inner.consecutive_empty_turns
        ),
        (false, false, 2)
    );
}
