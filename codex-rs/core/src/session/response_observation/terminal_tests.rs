use super::AgentResponseEvent;
use super::AgentResponseSubscription;
use super::AgentStatus;
use super::TerminalStatusEvent;
use super::TerminalStatusSubscription;
use crate::agent::control::TerminalPresentationOutcome;
use crate::session::tests::make_session_and_context;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use futures::FutureExt;
use pretty_assertions::assert_eq;
use std::sync::Weak;
use tokio::sync::mpsc;

#[tokio::test]
async fn terminal_subscription_skips_progress_and_retains_each_turn_outcome() {
    let (sender, receiver) = mpsc::unbounded_channel();
    let mut subscription = TerminalStatusSubscription {
        responses: AgentResponseSubscription {
            id: 0,
            state: Weak::new(),
            receiver,
        },
    };
    for event in [
        AgentResponseEvent::TurnStarted {
            turn_id: "first".to_string(),
            sequence: 0,
        },
        AgentResponseEvent::Commentary {
            turn_id: "first".to_string(),
            item_id: "progress".to_string(),
            text: "Still working".to_string(),
            sequence: 1,
        },
    ] {
        sender.send(event).expect("publish progress");
    }
    assert_eq!(subscription.recv().now_or_never(), None);

    let completed = AgentStatus::Completed(Some("first result".to_string()));
    for event in [
        AgentResponseEvent::Terminal {
            turn_id: "first".to_string(),
            status: completed.clone(),
        },
        AgentResponseEvent::TurnStarted {
            turn_id: "second".to_string(),
            sequence: 2,
        },
        AgentResponseEvent::TurnAborted {
            turn_id: "second".to_string(),
        },
    ] {
        sender.send(event).expect("publish turn transitions");
    }
    for (turn_id, status) in [("first", completed), ("second", AgentStatus::Interrupted)] {
        assert_eq!(
            subscription.recv().await,
            Some(TerminalStatusEvent {
                turn_id: Some(turn_id.to_string()),
                status,
            }),
        );
    }
    drop(sender);
    assert_eq!(subscription.recv().await, None);
}

#[tokio::test]
async fn interrupted_new_turn_does_not_adopt_an_older_guardian_terminal() {
    let (session, _turn) = make_session_and_context().await;
    assert!(session.begin_agent_response_turn("older"));
    let stopped = TerminalPresentationOutcome {
        status: AgentStatus::Errored("safety stop".to_string()),
        error_info: Some(CodexErrorInfo::TooManyDenials),
    };
    assert!(session.begin_agent_response_turn("newer"));
    {
        let _terminal = session
            .terminal_publication_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        session.record_agent_response_terminal_observers("older", stopped.clone());
        session.publish_agent_response_terminal("older".to_string(), stopped.status);
    }
    session
        .send_event_raw(Event {
            id: "newer".to_string(),
            msg: EventMsg::TurnAborted(TurnAbortedEvent {
                root_turn_id: None,
                turn_id: Some("newer".to_string()),
                reason: TurnAbortReason::Interrupted,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            }),
        })
        .await;

    let (terminal, _subscription) = session.subscribe_terminal_status_events();
    assert_eq!(
        terminal,
        TerminalStatusEvent {
            turn_id: None,
            status: AgentStatus::Interrupted,
        },
    );
}
