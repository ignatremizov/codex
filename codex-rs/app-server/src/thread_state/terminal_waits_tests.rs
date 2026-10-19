use super::super::ThreadState;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TerminalInteractionEvent;
use codex_protocol::protocol::TerminalWaitCompletionReason;
use codex_protocol::protocol::TerminalWaitEvent;
use codex_protocol::protocol::TerminalWaitMode;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnStartedEvent;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn resume_snapshot_keeps_only_current_unfinished_waits_and_original_timing() {
    let mut state = ThreadState::default();
    let thread_id = ThreadId::new();
    let snapshot = |state: &ThreadState| {
        serde_json::to_value(state.terminal_waits.snapshot(thread_id))
            .expect("serialize wait snapshot")
    };
    state.track_current_turn_event(
        "turn",
        &EventMsg::TurnStarted(TurnStartedEvent {
            turn_attribution: None,
            turn_id: "turn".into(),
            root_turn_id: None,
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
            agent_queue: None,
        }),
    );
    let mut started = TerminalInteractionEvent {
        call_id: "command".into(),
        process_id: "123".into(),
        stdin: String::new(),
        deadline_at_ms: Some(601_000),
        wait: Some(TerminalWaitEvent::Started {
            interaction_id: "wait".into(),
            started_at_ms: 1_000,
            mode: TerminalWaitMode::Timed,
        }),
    };
    state.track_current_turn_event("turn", &EventMsg::TerminalInteraction(started.clone()));
    let original = json!([{
        "method": "item/commandExecution/terminalInteraction",
        "params": {
            "threadId": thread_id.to_string(),
            "turnId": "turn",
            "itemId": "command",
            "processId": "123",
            "stdin": "",
            "deadlineAtMs": 601_000,
            "wait": {
                "phase": "started",
                "interactionId": "wait",
                "startedAtMs": 1_000,
                "mode": "timed"
            }
        }
    }]);
    assert_eq!(snapshot(&state), original);
    started.deadline_at_ms = Some(999_000);
    state.track_current_turn_event("turn", &EventMsg::TerminalInteraction(started.clone()));
    assert_eq!(snapshot(&state), original);

    let mut finished = TerminalInteractionEvent {
        call_id: "wrong-command".into(),
        process_id: "123".into(),
        stdin: String::new(),
        deadline_at_ms: None,
        wait: Some(TerminalWaitEvent::Finished {
            interaction_id: "wait".into(),
            elapsed_ms: 5_000,
            reason: TerminalWaitCompletionReason::Timeout,
        }),
    };
    state.track_current_turn_event("turn", &EventMsg::TerminalInteraction(finished.clone()));
    assert_eq!(snapshot(&state), original);
    finished.call_id = "command".into();
    state.track_current_turn_event("turn", &EventMsg::TerminalInteraction(finished));
    assert_eq!(snapshot(&state), json!([]));

    state.track_current_turn_event("turn", &EventMsg::TerminalInteraction(started));
    state.track_current_turn_event(
        "turn",
        &EventMsg::TurnAborted(TurnAbortedEvent {
            root_turn_id: None,
            turn_id: Some("turn".into()),
            reason: TurnAbortReason::Interrupted,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        }),
    );
    assert_eq!(snapshot(&state), json!([]));
}
