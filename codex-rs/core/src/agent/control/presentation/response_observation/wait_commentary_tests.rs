use super::*;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[derive(Clone, Copy)]
enum WaitExit {
    Cancelled,
    TimedOut,
    Completed,
}

#[test_case(WaitExit::Cancelled; "cancelled")]
#[test_case(WaitExit::TimedOut; "timed_out")]
#[test_case(WaitExit::Completed; "completed")]
#[tokio::test]
async fn active_wait_holds_early_commentary_until_its_last_owner_exits(exit: WaitExit) {
    let control = LocalAgentControl::default();
    let parent = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let child = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let foreign_parent = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let turn_id = "child-turn";
    control
        .wait_agent_presentations
        .state()
        .response_observation_by_observer_child
        .insert(
            (parent, child),
            ResponseObserverRelationship {
                turns: HashMap::from([(
                    turn_id.to_owned(),
                    ResponseTurnObservation {
                        commentary_admissions: vec![AgentResponseCommentaryAdmission {
                            minimum_event_sequence: 12,
                            after_item_id: None,
                            canonical_boundary: true,
                        }],
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
        );
    let first = control
        .wait_agent_presentations
        .register(parent, Some(HashSet::from([child.thread_id])));
    let second = control
        .wait_agent_presentations
        .register(parent, Some(HashSet::from([child.thread_id])));
    let _foreign_wait = control
        .wait_agent_presentations
        .register(foreign_parent, Some(HashSet::from([child.thread_id])));
    let _sibling_wait = control
        .wait_agent_presentations
        .register(parent, Some(HashSet::from([ThreadId::new()])));
    control
        .prepare_commentary_observation_delivery_at_sequence(
            parent,
            child,
            turn_id,
            "early-commentary",
            "progress before completion",
            /*sequence*/ 12,
        )
        .expect("eligible commentary");
    assert_eq!(
        control.route_response_observer_commentary(parent, child, turn_id),
        CommentaryDeliveryRoute::Wait,
    );
    drop(first);
    assert_eq!(
        control.route_response_observer_commentary(parent, child, turn_id),
        CommentaryDeliveryRoute::Wait,
        "another matching wait still owns the admission hold",
    );
    let changed = control.response_observation_changed().notified();
    tokio::pin!(changed);
    changed.as_mut().enable();
    match exit {
        WaitExit::Cancelled => drop(second),
        WaitExit::TimedOut => drop(second.freeze_for_terminal_statuses(&HashMap::new())),
        WaitExit::Completed => second
            .freeze_for_terminal_statuses(&HashMap::new())
            .commit(),
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), changed)
        .await
        .expect("last matching wait must wake the commentary publisher");
    assert_eq!(
        control.route_response_observer_commentary(parent, child, turn_id),
        CommentaryDeliveryRoute::Mailbox,
        "unrelated waits must not retain the commentary",
    );
    let late = control
        .wait_agent_presentations
        .register(parent, Some(HashSet::from([child.thread_id])));
    assert_eq!(
        control.route_response_observer_commentary(parent, child, turn_id),
        CommentaryDeliveryRoute::Mailbox,
        "a later wait cannot steal an already selected delivery",
    );
    drop(late);
}
