//! Prompt publication retains captured input metadata and settles only its canonical receipt.

use super::*;
use crate::session::UserInputMetadata;
use crate::session::tests::attach_in_memory_thread_store;
use crate::session::tests::make_session_and_context_with_rx;
use codex_history::RolloutItem;
use codex_history::UserInputOrigin;
use codex_protocol::protocol::AgentQueueTurnMetadata;
use codex_protocol::protocol::EventMsg;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::ThreadStore;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use test_case::test_case;

#[test_case(UserInputOrigin::User; "human prompt")]
#[test_case(UserInputOrigin::Heartbeat; "captured heartbeat prompt")]
#[tokio::test]
async fn prompt_publication_retains_origin_order_client_identity_and_queue_receipt(
    origin: UserInputOrigin,
) {
    let (mut session, turn, events) = make_session_and_context_with_rx().await;
    let store =
        attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique session")).await;
    let queued = AgentQueueTurnMetadata {
        queue_id: "queued-input".to_string(),
        source_thread_id: session.thread_id,
        response_handling: None,
    };
    let (permit, mut persisted) = session.register_queued_input_start("submission", queued.clone());
    let resolution = session.capture_input_turn_admission_resolution(turn.sub_id.clone());
    session.resolve_input_turn_admission("submission", resolution);
    permit.publish();
    assert_eq!(
        session.await_agent_queue_turn_metadata(&turn.sub_id).await,
        Some(queued)
    );
    assert!(persisted.try_recv().is_err());

    let input = vec![UserInput::Text {
        text: "captured prompt".to_string(),
        text_elements: Vec::new(),
    }];
    session
        .record_prompt_and_emit_turn_item(
            &turn,
            turn.model_info(),
            &input,
            PersistContext::Standard,
            PromptInputKind::User {
                client_id: Some("client-submission".to_string()),
                metadata: UserInputMetadata {
                    acceptance_order: Some(17),
                    origin,
                },
            },
            Vec::new(),
        )
        .await
        .expect("canonical prompt publication");
    persisted
        .try_recv()
        .expect("positive prompt receipt")
        .expect("successful publication");

    let history = store
        .load_history(LoadThreadHistoryParams {
            thread_id: session.thread_id,
            include_archived: false,
        })
        .await
        .expect("canonical source history");
    let canonical = history
        .items
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(item) => Some(item.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        session.state.lock().await.history.annotated_items(),
        canonical.as_slice()
    );
    let [envelope] = canonical.as_slice() else {
        panic!("one canonical input");
    };
    let ResponseItem::Message {
        role,
        content,
        internal_chat_message_metadata_passthrough: Some(passthrough),
        ..
    } = &envelope.item
    else {
        panic!("stamped prompt source");
    };
    assert_eq!(
        (role.as_str(), content),
        (
            "user",
            &vec![ContentItem::InputText {
                text: "captured prompt".to_string(),
            }]
        )
    );
    assert_eq!(
        envelope
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.user_input_order),
        Some(17)
    );
    if origin == UserInputOrigin::Heartbeat {
        assert_eq!(
            passthrough.content_item_kinds,
            Some(vec![ContentItemKind(
                codex_history::HEARTBEAT_CONTENT_KIND.to_string(),
            )])
        );
    }
    let mut published = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EventMsg::ItemCompleted(item) = event.msg {
            published.push(item.item);
        }
    }
    let [TurnItem::UserMessage(actual)] = published.as_slice() else {
        panic!("one typed user prompt");
    };
    let mut expected = UserMessageItem::new(&input);
    expected.id.clone_from(&actual.id);
    expected.client_id = Some("client-submission".to_string());
    assert_eq!(
        serde_json::to_value(actual).expect("serialize actual prompt"),
        serde_json::to_value(&expected).expect("serialize expected prompt"),
    );
    assert!(session.check_history_publication().is_ok());
}

#[test_case(false; "delegated user presentation")]
#[test_case(true; "attributed agent presentation")]
#[tokio::test]
async fn agent_prompt_keeps_internal_route_out_of_the_typed_visible_input(attributed: bool) {
    let (mut session, turn, events) = make_session_and_context_with_rx().await;
    let store =
        attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique session")).await;
    let visible = vec![UserInput::Text {
        text: "the authored task".to_string(),
        text_elements: Vec::new(),
    }];
    let mut input = visible.clone();
    input.push(UserInput::Text {
        text: "internal scoped route".to_string(),
        text_elements: Vec::new(),
    });
    let presentation = if attributed {
        AgentInputPresentation::Attributed("attributed task".to_string())
    } else {
        AgentInputPresentation::Delegated(visible.clone())
    };
    session
        .record_prompt_and_emit_turn_item(
            &turn,
            turn.model_info(),
            &input,
            PersistContext::Standard,
            PromptInputKind::Agent {
                presentation,
                metadata: UserInputMetadata {
                    acceptance_order: Some(23),
                    ..Default::default()
                },
            },
            Vec::new(),
        )
        .await
        .expect("publish attributed input");
    let history = store
        .load_history(LoadThreadHistoryParams {
            thread_id: session.thread_id,
            include_archived: false,
        })
        .await
        .expect("canonical source history");
    let canonical = history
        .items
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(item) => Some(item.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        session.state.lock().await.history.annotated_items(),
        canonical.as_slice()
    );
    assert_eq!(canonical.len(), 1);
    assert_eq!(
        crate::context::is_user_authorization_message(&canonical[0].item),
        !attributed
    );
    assert_eq!(
        canonical[0]
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.user_input_order),
        Some(23)
    );
    let mut published = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EventMsg::ItemCompleted(item) = event.msg {
            published.push(item.item);
        }
    }
    let [actual] = published.as_slice() else {
        panic!("one attributed prompt");
    };
    let expected = if attributed {
        let mut item = AgentMessageItem::new(&[AgentMessageContent::Text {
            text: "attributed task".to_string(),
        }]);
        item.id = actual.id();
        item.phase = Some(MessagePhase::Commentary);
        TurnItem::AgentMessage(item)
    } else {
        let mut item = UserMessageItem::new(&visible);
        item.id = actual.id();
        TurnItem::UserMessage(item)
    };
    assert_eq!(
        serde_json::to_value(actual).expect("serialize actual presentation"),
        serde_json::to_value(&expected).expect("serialize expected presentation"),
    );
}

#[path = "prompt_input_attributed_tests.rs"]
mod attributed_tests;
