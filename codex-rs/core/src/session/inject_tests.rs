use crate::session::tests::make_session_and_context_with_rx;
use codex_features::Feature;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::models::ConfigurationReasoning;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::EventMsg;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn public_agent_injection_owns_identity_before_idle_or_active_acceptance() {
    for running in [false, true] {
        let (session, turn_context, events) = make_session_and_context_with_rx().await;
        let input = ResponseItem::AgentMessage {
            id: Some(codex_protocol::ResponseItemId::with_suffix(
                "msg",
                "untrusted",
            )),
            author: "/root".into(),
            recipient: "/root/worker".into(),
            content: vec![
                codex_protocol::models::AgentMessageInputContent::InputText {
                    text: "new public message".into(),
                },
            ],
            internal_chat_message_metadata_passthrough: Some(
                codex_protocol::models::InternalChatMessageMetadataPassthrough {
                    turn_id: Some("untrusted-turn".into()),
                    ..Default::default()
                },
            ),
        };
        let active = crate::state::ActiveTurn::default();
        let turn_state = std::sync::Arc::clone(&active.turn_state);
        if running {
            *session.active_turn.lock().await = Some(active);
        }
        session
            .inject_client_response_items(vec![input.clone()], &turn_context)
            .await
            .expect("public injection");
        if running {
            assert!(
                events.try_recv().is_err(),
                "queue acceptance is not publication"
            );
            let queued = session
                .input_queue
                .take_pending_input_for_turn_state(&turn_state)
                .await;
            let [crate::session::TurnInput::ResponseItem(envelope)] = queued.as_slice() else {
                panic!("expected one accepted response item");
            };
            assert!(envelope.item.id().is_some_and(|id| id.starts_with("amsg_")));
            assert_eq!(envelope.item.turn_id(), None);
            session
                .record_annotated_conversation_items(
                    &turn_context,
                    turn_context.model_info(),
                    vec![envelope.clone()],
                )
                .await;
        }
        let event = events.try_recv().expect("publication enqueued raw event");
        let EventMsg::RawResponseItem(raw) = event.msg else {
            panic!("history-only injection must not emit runtime turn boundaries");
        };
        assert!(raw.item.id().is_some_and(|id| id.starts_with("amsg_")));
        assert_eq!(raw.item.turn_id(), Some(turn_context.sub_id.as_str()));
        let mut expected = input;
        expected.set_id(raw.item.id().cloned());
        expected.clear_internal_chat_message_metadata_passthrough();
        let mut actual = raw.item.clone();
        actual.clear_internal_chat_message_metadata_passthrough();
        assert_eq!(actual, expected);
        assert_eq!(
            session.clone_history().await.into_annotated_items(),
            vec![raw.item.into()]
        );
        assert!(events.try_recv().is_err());
    }
}

#[tokio::test]
async fn trusted_agent_publication_preserves_existing_identity_and_turn() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    let mut expected = ResponseItem::AgentMessage {
        id: Some(codex_protocol::ResponseItemId::with_suffix(
            "amsg", "trusted",
        )),
        author: "/root".into(),
        recipient: "/root/worker".into(),
        content: vec![
            codex_protocol::models::AgentMessageInputContent::InputText {
                text: "trusted message".into(),
            },
        ],
        internal_chat_message_metadata_passthrough: None,
    };
    expected.set_turn_id_if_missing("original-turn");
    expected.set_create_time_if_missing(serde_json::Number::from(123));
    session
        .record_conversation_items(
            &turn_context,
            turn_context.model_info(),
            std::slice::from_ref(&expected),
        )
        .await;
    let event = events.try_recv().expect("published");
    let EventMsg::RawResponseItem(raw) = event.msg else {
        panic!("raw item");
    };
    assert_eq!(raw.item, expected);
    assert_eq!(
        session.clone_history().await.into_annotated_items(),
        vec![expected.into()]
    );
}

#[tokio::test]
async fn harness_authored_configuration_updates_preserve_metadata_and_resume() {
    let (session, turn_context, rx_event) = make_session_and_context_with_rx().await;
    assert!(!session.enabled(Feature::RetainClientDeveloperMessages));

    let expected = ResponseItemEnvelope {
        item: ResponseItem::ConfigurationUpdate {
            reasoning: ConfigurationReasoning {
                effort: ReasoningEffort::High,
            },
        },
        metadata: Some(CodexHarnessMetadata {
            harness_authored_configuration: true,
            ..Default::default()
        }),
    };
    session
        .record_annotated_conversation_items(
            &turn_context,
            turn_context.model_info(),
            vec![expected.clone()],
        )
        .await;

    let recorded = session.clone_history().await.into_annotated_items();
    assert_eq!(recorded, vec![expected.clone()]);
    let mut raw_items = Vec::new();
    while let Ok(event) = rx_event.try_recv() {
        if let EventMsg::RawResponseItem(event) = event.msg {
            raw_items.push(event.item);
        }
    }
    assert_eq!(raw_items, vec![expected.item]);

    let rollout_items = recorded
        .iter()
        .cloned()
        .map(RolloutItem::ResponseItem)
        .collect::<Vec<_>>();
    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;
    assert_eq!(reconstructed.history, recorded);
}
