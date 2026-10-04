//! Explicit-use ordering must remain authoritative for instant-input observers.

use super::*;
use crate::context::ContextualUserFragment;
use crate::context::McpServerUseInstructions;
use pretty_assertions::assert_eq;

#[test]
fn instant_input_detection_stops_at_the_explicit_activation_boundary() {
    let user = TurnInput::UserInput {
        metadata: UserInputMetadata {
            acceptance_order: Some(7),
            ..Default::default()
        },
        content: vec![UserInput::Text {
            text: "the next user turn".to_string(),
            text_elements: Vec::new(),
        }],
        client_id: Some("client-next".to_string()),
    };
    let boundary =
        TurnInput::ResponseItem(ResponseItemEnvelope::new(ContextualUserFragment::into(
            McpServerUseInstructions::new("docs".to_string(), "[]".to_string()),
        )));
    let mut queue = TurnInputQueue::default();
    queue.append_to_front(vec![boundary.clone(), user.clone()]);
    assert!(!queue.has_user_input());
    assert_eq!(queue.as_slice(), &[boundary.clone(), user.clone()]);

    // An earlier eligible input still interrupts. Removing it must not allow
    // the later input to leap over the retained explicit-use boundary.
    let before = TurnInput::UserInput {
        metadata: UserInputMetadata {
            acceptance_order: Some(6),
            ..Default::default()
        },
        content: vec![UserInput::Text {
            text: "an earlier eligible input".to_string(),
            text_elements: Vec::new(),
        }],
        client_id: Some("client-before".to_string()),
    };
    queue.append_to_front(vec![before.clone()]);
    assert!(queue.has_user_input());
    let items = queue.take();
    assert_eq!(items, vec![before, boundary, user.clone()]);
    queue.append_to_front(items.into_iter().skip(/*n*/ 1).collect());
    assert!(!queue.has_user_input());

    // After the boundary has been consumed, the original input is eligible,
    // retaining its acceptance and client identities.
    let following = queue.take().into_iter().skip(/*n*/ 1).collect();
    queue.append_to_front(following);
    assert!(queue.has_user_input());
    assert_eq!(queue.as_slice(), &[user]);
}

fn response_injection(activity: InputQueueActivity) -> TurnInput {
    let item = match activity {
        InputQueueActivity::Steer => ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![codex_protocol::models::input_text("injected")],
            phase: None,
            status: None,
            encrypted_content: None,
            internal_chat_message_metadata_passthrough: None,
        },
        InputQueueActivity::Mailbox => ResponseItem::AgentMessage {
            id: None,
            author: "/root/worker".to_string(),
            recipient: "/root".to_string(),
            content: vec![
                codex_protocol::models::AgentMessageInputContent::InputText {
                    text: "native agent mail".to_string(),
                },
            ],
            internal_chat_message_metadata_passthrough: None,
        },
    };
    TurnInput::ResponseItem(ResponseItemEnvelope::new(item))
}

#[tokio::test]
async fn partial_boundary_drain_tracks_the_remaining_notifying_injection() {
    for (activity, injection_after_boundary) in [
        (InputQueueActivity::Steer, false),
        (InputQueueActivity::Steer, true),
        (InputQueueActivity::Mailbox, false),
        (InputQueueActivity::Mailbox, true),
    ] {
        let input_queue = InputQueue::new();
        let active_turn = ActiveTurn::default();
        let state = Arc::clone(&active_turn.turn_state);
        let active = Mutex::new(Some(active_turn));
        let injection = response_injection(activity);
        let boundary =
            TurnInput::ResponseItem(ResponseItemEnvelope::new(ContextualUserFragment::into(
                McpServerUseInstructions::new("docs".to_string(), "[]".to_string()),
            )));
        if injection_after_boundary {
            input_queue
                .extend_pending_input_for_turn_state(&state, vec![boundary.clone()])
                .await;
        }
        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &state,
                vec![injection.clone()],
            )
            .await;
        if !injection_after_boundary {
            input_queue
                .extend_pending_input_for_turn_state(&state, vec![boundary.clone()])
                .await;
        }

        let drained = input_queue.get_pending_input(&active).await.0;
        assert_eq!(
            (
                drained,
                input_queue.subscribe_activity(Some(&state)).await.1,
                input_queue.take_pending_input_for_turn_state(&state).await,
            ),
            if injection_after_boundary {
                (
                    Vec::new(),
                    Some(activity),
                    vec![boundary.clone(), injection],
                )
            } else {
                (vec![injection], None, vec![boundary.clone()])
            },
        );
        input_queue
            .extend_pending_input_for_turn_state(&state, vec![boundary])
            .await;
        assert_eq!(input_queue.subscribe_activity(Some(&state)).await.1, None);
    }
}

#[tokio::test]
async fn partial_boundary_drain_preserves_the_remaining_notification_kind() {
    for (before_activity, after_activity) in [
        (InputQueueActivity::Steer, InputQueueActivity::Mailbox),
        (InputQueueActivity::Mailbox, InputQueueActivity::Steer),
    ] {
        let input_queue = InputQueue::new();
        let active_turn = ActiveTurn::default();
        let state = Arc::clone(&active_turn.turn_state);
        let active = Mutex::new(Some(active_turn));
        let before = response_injection(before_activity);
        let after = response_injection(after_activity);
        let boundary =
            TurnInput::ResponseItem(ResponseItemEnvelope::new(ContextualUserFragment::into(
                McpServerUseInstructions::new("docs".to_string(), "[]".to_string()),
            )));
        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &state,
                vec![before.clone()],
            )
            .await;
        input_queue
            .extend_pending_input_for_turn_state(&state, vec![boundary.clone()])
            .await;
        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &state,
                vec![after.clone()],
            )
            .await;
        assert_eq!(
            input_queue.subscribe_activity(Some(&state)).await.1,
            Some(InputQueueActivity::Steer)
        );

        assert_eq!(input_queue.get_pending_input(&active).await.0, vec![before]);
        assert_eq!(
            input_queue.subscribe_activity(Some(&state)).await.1,
            Some(after_activity)
        );
        assert_eq!(
            input_queue.take_pending_input_for_turn_state(&state).await,
            vec![boundary, after]
        );
        assert_eq!(input_queue.subscribe_activity(Some(&state)).await.1, None);
    }
}
