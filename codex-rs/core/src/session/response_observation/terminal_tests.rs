use super::AgentResponseEvent;
use super::AgentResponseSubscription;
use super::AgentStatus;
use super::TerminalStatusEvent;
use super::TerminalStatusSubscription;
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
