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
