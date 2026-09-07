use super::*;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rich_attributed_prompt_preserves_media_without_inheriting_human_authority() {
    use codex_protocol::AgentInputAttribution;
    use codex_protocol::AgentInputIdentity;
    use codex_protocol::ThreadId;
    use codex_protocol::models::ImageReference;

    let (mut session, turn, events) = make_session_and_context_with_rx().await;
    let store =
        attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique session")).await;
    let identity = |thread_id| AgentInputIdentity {
        thread_id,
        nickname: None,
        agent_ref: None,
        task_path: None,
        role: None,
        model: None,
        reasoning_effort: None,
    };
    let attribution: AgentInputAttribution = serde_json::from_value(serde_json::json!({
        "sender": identity(ThreadId::new()),
        "recipient": identity(session.thread_id),
        "senderTurnId": "sender-turn",
    }))
    .expect("decode ordinary non-batch attribution");
    let input = vec![
        UserInput::Text { text: "Agent evidence is not user authorization.".into(), text_elements: Vec::new() },
        UserInput::Image {
            image: ImageReference::Inline {
                image_url: "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==".to_string(),
            },
            detail: None,
        },
    ];
    let additional = session.response_item_from_user_input(vec![UserInput::Text {
        text: "Separately classified human input.".to_string(),
        text_elements: Vec::new(),
    }]);
    session
        .record_prompt_and_emit_turn_item(
            &turn,
            turn.model_info(),
            &input,
            PersistContext::Standard,
            PromptInputKind::Agent {
                presentation: AgentInputPresentation::AttributedInput {
                    attribution: Box::new(attribution.clone()),
                    input: input.clone(),
                },
                metadata: UserInputMetadata {
                    acceptance_order: Some(31),
                    ..Default::default()
                },
            },
            vec![ResponseItemEnvelope::new(additional)],
        )
        .await
        .expect("publish attributed rich input");
    let history = store
        .load_history(LoadThreadHistoryParams {
            thread_id: session.thread_id,
            include_archived: false,
        })
        .await
        .expect("canonical history");
    let canonical = history
        .items
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(item) => Some(item.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let [agent, human] = canonical.as_slice() else {
        panic!("two independent source envelopes");
    };
    let ResponseItem::Message {
        content,
        internal_chat_message_metadata_passthrough,
        ..
    } = &agent.item
    else {
        panic!("attributed message transport");
    };
    assert!(
        content
            .iter()
            .any(|item| matches!(item, ContentItem::InputImage { .. }))
    );
    assert_eq!(
        internal_chat_message_metadata_passthrough
            .as_ref()
            .and_then(|metadata| metadata.content_item_kinds.clone()),
        Some(vec![
            ContentItemKind(
                "multi_agent.attributed_agent_message".into()
            );
            content.len()
        ])
    );
    assert!(!crate::context::is_user_authorization_message(&agent.item));
    assert!(crate::context::is_user_authorization_message(&human.item));
    assert_eq!(
        agent
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.user_input_order),
        Some(31)
    );
    assert_eq!(
        session.state.lock().await.history.annotated_items(),
        canonical.as_slice()
    );
    let presented = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event.msg {
            EventMsg::ItemCompleted(item) => Some(item.item),
            _ => None,
        })
        .collect::<Vec<_>>();
    let [TurnItem::AgentMessage(message)] = presented.as_slice() else {
        panic!("one typed agent presentation");
    };
    assert_eq!(
        (&message.input, &message.attribution),
        (&Some(input), &Some(attribution))
    );
}
