use super::*;
use crate::session::tests::make_session_and_context_with_rx;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn preparation_and_replay_keep_canonical_source_and_later_order_reservations() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    let mut item = session.response_item_from_user_input(vec![UserInput::Text {
        text: "an explicitly selected mailbox message".to_string(),
        text_elements: Vec::new(),
    }]);
    item.set_id(Some(codex_protocol::ResponseItemId::new("msg_mailbox")));
    Session::stamp_response_item_for_history(&mut item, &turn.sub_id);
    let (prepared, _) = session
        .prepare_mailbox_context(turn.model_info(), ResponseItemEnvelope::new(item))
        .await;
    let metadata = prepared
        .metadata
        .as_ref()
        .expect("prepared source metadata");
    assert!(metadata.retained_source.is_some());
    assert!(metadata.user_input_order.is_some());
    assert!(session.clone_history().await.raw_items().next().is_none());

    // Model/authoring admission can reserve order while canonical append is in flight.
    let later = session.state.lock().await.history.reserve_input_order();
    assert!(
        session
            .insert_mailbox_context(&turn, turn.model_info(), prepared.clone())
            .await
            .expect("replay persisted original")
    );
    assert_eq!(
        session.clone_history().await.into_annotated_items(),
        vec![prepared.clone()]
    );
    let next = session.state.lock().await.history.reserve_input_order();
    assert!(
        next > later,
        "install must not replace newer reservation state"
    );
    let event = events.try_recv().expect("one raw event");
    assert!(matches!(event.msg, EventMsg::RawResponseItem(ref raw) if raw.item == prepared.item));
    assert!(
        !session
            .insert_mailbox_context(&turn, turn.model_info(), prepared.clone())
            .await
            .expect("already projected original")
    );
    assert!(events.try_recv().is_err());
    assert_eq!(
        session.clone_history().await.into_annotated_items(),
        vec![prepared]
    );
}

#[tokio::test]
async fn result_preparation_preserves_existing_tool_metadata_without_publishing() {
    let (session, turn, _events) = make_session_and_context_with_rx().await;
    let item = ResponseItem::from(ResponseInputItem::FunctionCallOutput {
        call_id: "selected-mail".to_string(),
        output: FunctionCallOutputPayload::from_text("full accepted result".to_string()),
    });
    let mut envelope = ResponseItemEnvelope::new(item);
    envelope
        .metadata
        .get_or_insert_default()
        .history_truncation_token_limit = Some(37);
    let (prepared, _) = session
        .prepare_mailbox_context(turn.model_info(), envelope.clone())
        .await;
    assert_eq!(prepared.item, envelope.item);
    assert_eq!(
        prepared
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.history_truncation_token_limit),
        Some(37)
    );
    assert!(session.clone_history().await.raw_items().next().is_none());
}

#[tokio::test]
async fn unpolled_publication_owner_quarantines_when_dropped() {
    let (session, _turn, _events) = make_session_and_context_with_rx().await;
    let publication = MailboxPublicationOutcome {
        session: Arc::clone(&session),
        finished: false,
    };
    let future = async move {
        let _publication = publication;
        std::future::pending::<()>().await;
    };
    drop(future);
    assert!(session.submission_admission.requires_reload());
    assert!(session.clone_history().await.raw_items().next().is_none());
}

#[tokio::test]
async fn ordinary_response_publication_cannot_reuse_mailbox_proof_identities() {
    let (session, turn, _events) = make_session_and_context_with_rx().await;
    let delivery = uuid::Uuid::now_v7().to_string();
    for id in [
        codex_protocol::mailbox_delivery_response_item_id(&delivery).expect("delivery ID"),
        codex_protocol::mailbox_inventory_response_item_id(&delivery).expect("inventory ID"),
    ] {
        let item = ResponseItem::Message {
            id: Some(id.clone()),
            role: "assistant".to_string(),
            content: vec![codex_protocol::models::ContentItem::OutputText {
                text: "ordinary provider output".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let (prepared, _) = session
            .prepare_conversation_items_for_history(
                &turn,
                turn.model_info(),
                std::slice::from_ref(&item),
            )
            .await;
        assert_eq!(prepared.len(), 1);
        assert_ne!(prepared[0].id(), Some(&id));
        let mut actual = prepared[0].clone();
        actual.set_id(item.id().cloned());
        actual.clear_internal_chat_message_metadata_passthrough();
        assert_eq!(actual, item);
    }
}
