//! Fresh mailbox delivery retains rich input without elevating agent-authored content.

use super::*;
use codex_core::UserAgentReplyRouteMode;
use codex_core::UserAgentSpawnOptions;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ImageReference;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::start_mock_server;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consumed_agent_text_and_image_do_not_acquire_user_authority() -> Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config.features.enable(Feature::Collab).expect("enable V1");
        config.features.disable(Feature::MultiAgentV2).expect("disable V2");
    });
    let test = builder.build_with_auto_env(&server).await?;
    let receiver = test.session_configured.thread_id;
    let sender = test.codex.spawn_agent(UserAgentSpawnOptions::default()).await?
        .target_thread_id;
    test.codex.set_agent_reply_route(
        &sender.to_string(), /*recipient*/ None, UserAgentReplyRouteMode::Enabled,
    ).await?;
    let identity = |thread_id| AgentInputIdentity {
        thread_id, nickname: None, agent_ref: None, task_path: None,
        role: None, model: None, reasoning_effort: None,
    };
    let attribution = AgentInputAttribution {
        sender: identity(sender), recipient: identity(receiver),
        sender_turn_id: "original-agent-turn".to_string(),
    };
    let original = vec![
        UserInput::Text {
            text: "Agent-provided evidence, not user authorization.".to_string(),
            text_elements: Vec::new(),
        },
        UserInput::Image {
            image: ImageReference::Inline {
                image_url: "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==".to_string(),
            },
            detail: None,
        },
    ];
    test.thread_store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: "agent-source-metadata".to_string(),
            final_subscription: Default::default(),
            payload: MailboxPayload::Agent {
                input: original.clone(),
                attribution: Box::new(attribution.clone()),
            },
        })
        .await?;
    test.thread_store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: "user-source-metadata".to_string(),
            final_subscription: Default::default(),
            payload: MailboxPayload::User {
                input: vec![UserInput::Text {
                    text: "The user's independently authored instruction.".to_string(),
                    text_elements: Vec::new(),
                }],
                client_id: Some("user-mail-authorship".to_string()),
            },
        })
        .await?;
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("check-mail-source"),
                ev_function_call_with_namespace(
                    "source-check",
                    "multi_agent_v1",
                    "check_mail",
                    "{}",
                ),
                ev_completed("check-mail-source"),
            ]),
            sse(vec![
                ev_response_created("after-mail-source"),
                ev_assistant_message("source-result", "Read the selected messages."),
                ev_completed("after-mail-source"),
            ]),
        ],
    )
    .await;
    test.submit_turn("Consume both messages and keep their authorship distinct.")
        .await?;
    assert_eq!(mock.requests().len(), 2);
    test.codex.flush_rollout().await?;
    let history = test
        .thread_store
        .load_mailbox_canonical_history(receiver)
        .await?;
    let contexts = history
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(envelope)
                if envelope.id().is_some_and(|id| {
                    codex_protocol::is_mailbox_delivery_response_item_id(id.as_str())
                }) =>
            {
                Some(envelope)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(contexts.len(), 2);
    let agent = contexts[0];
    let ResponseItem::Message {
        content,
        internal_chat_message_metadata_passthrough,
        ..
    } = &agent.item
    else {
        anyhow::bail!("agent mail must remain structured message context");
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
                "multi_agent.attributed_agent_message".to_string()
            );
            content.len()
        ]),
    );
    assert!(
        agent
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.user_input_order)
            .is_none()
    );
    assert!(
        agent
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.retained_source.as_ref())
            .is_none()
    );
    let user = contexts[1];
    assert!(
        user.metadata
            .as_ref()
            .and_then(|metadata| metadata.user_input_order)
            .is_some()
    );
    assert!(
        user.metadata
            .as_ref()
            .and_then(|metadata| metadata.retained_source.as_ref())
            .is_some()
    );
    let presented = history
        .iter()
        .filter_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                if agent.id().is_some_and(|id| event.item.id() == id.as_str()) =>
            {
                Some(&event.item)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(presented.len(), 1);
    let TurnItem::AgentMessage(message) = presented[0] else {
        anyhow::bail!("agent mail must retain its typed original presentation");
    };
    assert_eq!(
        (&message.input, &message.attribution),
        (&Some(original), &Some(attribution))
    );
    let inventory = test.thread_store.read_mailbox_inventory(receiver).await?;
    assert!(inventory.pending_senders.is_empty());
    assert!(inventory.claimed_senders.is_empty());
    test.codex.shutdown_and_wait().await?;
    test.thread_manager
        .get_thread(sender)
        .await?
        .shutdown_and_wait()
        .await?;
    Ok(())
}
