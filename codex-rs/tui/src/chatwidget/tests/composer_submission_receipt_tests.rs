//! Canonical identity receipts preserve admitted out-of-order steer reconciliation.

use super::*;
use crate::history_cell::UserHistoryCell;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn out_of_order_admitted_receipts_attach_identity_without_duplicate_optimistic_rows() {
    for optimistic in [false, true] {
        let (mut chat, mut events, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
        let thread_id = ThreadId::new();
        chat.thread_id = Some(thread_id);
        let content = vec![UserInput::Text {
            text: "same text".into(),
            text_elements: Vec::new(),
        }];
        chat.input_queue.pending_steers = VecDeque::from([
            PendingSteer {
                client_id: "first".into(),
                admitted_turn_id: Some("turn-one".into()),
                ..pending_steer("same text")
            },
            PendingSteer {
                client_id: "second".into(),
                admitted_turn_id: Some("turn-two".into()),
                optimistically_rendered: optimistic,
                ..pending_steer("same text")
            },
        ]);
        if optimistic {
            chat.on_identified_user_message_display(
                ChatWidget::user_message_display_from_inputs(&content),
                Some("second"),
                /*identity*/ None,
            );
            chat.last_rendered_user_message_client_id = Some("second".into());
        }
        while events.try_recv().is_ok() {}
        for repeated in [false, true] {
            chat.on_committed_user_message(
                &content,
                Some("second"),
                /*from_replay*/ false,
                "turn-two",
                "msg_second",
            );
            assert_eq!(
                chat.input_queue
                    .pending_steers
                    .iter()
                    .map(|pending| pending.client_id.as_str())
                    .collect::<Vec<_>>(),
                vec!["first"],
            );
            let mut identities = Vec::new();
            let mut rows = Vec::new();
            while let Ok(event) = events.try_recv() {
                match event {
                    AppEvent::AttachUserMessageIdentity {
                        thread_id,
                        identity,
                        client_id,
                        content,
                    } => {
                        identities.push((
                            thread_id,
                            identity.turn_id,
                            identity.item_id,
                            client_id,
                            content,
                        ));
                    }
                    AppEvent::InsertHistoryCell(cell) => {
                        if let Some(user) = cell.as_any().downcast_ref::<UserHistoryCell>() {
                            let identity =
                                user.identity.get().expect("new row has committed identity");
                            rows.push((
                                user.message.clone(),
                                user.client_id.clone(),
                                identity.turn_id.clone(),
                                identity.item_id.clone(),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            assert_eq!(
                identities,
                vec![(
                    thread_id,
                    "turn-two".into(),
                    "msg_second".into(),
                    Some("second".into()),
                    content.clone()
                )]
            );
            let expected_rows = if !optimistic && !repeated {
                vec![(
                    "same text".into(),
                    Some("second".into()),
                    "turn-two".into(),
                    "msg_second".into(),
                )]
            } else {
                Vec::new()
            };
            assert_eq!(rows, expected_rows);
        }
    }
}
