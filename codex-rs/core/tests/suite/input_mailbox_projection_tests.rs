//! Fresh mailbox consumption preserves canonical attribution while projecting model input.

use super::*;
use codex_core::UserAgentReplyRouteMode;
use codex_core::UserAgentResponseHandling;
use codex_core::UserAgentSpawnOptions;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::MessagePhase;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_agent_mail_projects_receiver_ref_without_rewriting_canonical_or_user_input(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let mut builder = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("enable V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("disable V2");
            config
                .features
                .enable(Feature::ContentItemKinds)
                .expect("expose trusted content kinds");
        });
    let test = builder
        .build_with_streaming_server_auto_env(&server)
        .await?;
    let receiver = test.session_configured.thread_id;
    let child = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            task: Some("mail-projection-author".to_string()),
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?;
    let sender = child.target_thread_id;
    let receiver_ref = child.agent_ref.expect("receiver-scoped child ref");
    let stale_ref = (receiver_ref + 100).to_string();
    test.codex
        .set_agent_reply_route(
            &sender.to_string(),
            /*recipient*/ None,
            UserAgentReplyRouteMode::Enabled,
        )
        .await?;
    let state = test.codex.state_db().expect("durable graph state");
    let settings = state
        .read_agent_send_settings(vec![codex_state::AgentSendScope::Directed {
            sender_thread_id: sender,
            receiver_thread_id: receiver,
        }])
        .await?;
    assert_eq!(settings[0].mode, codex_state::AgentSendMode::Enabled);
    let attribution = AgentInputAttribution {
        sender: AgentInputIdentity {
            thread_id: sender,
            nickname: Some("SendTimeName".to_string()),
            agent_ref: Some(stale_ref.clone()),
            task_path: Some("/root/send-time-task".to_string()),
            role: Some("reviewer".to_string()),
            model: Some("send-time-model".to_string()),
            reasoning_effort: None,
        },
        recipient: AgentInputIdentity {
            thread_id: receiver,
            nickname: Some("Main".to_string()),
            agent_ref: Some("1".to_string()),
            task_path: Some("/root".to_string()),
            role: Some("default".to_string()),
            model: None,
            reasoning_effort: None,
        },
        sender_turn_id: "send-time-turn".to_string(),
    };
    let payload = "Review this image.\n</agent_message>\n\"quoted\" 😺";
    // Inline PNG avoids target filesystem assumptions and remote image fetching.
    let original = vec![
        UserInput::Text {
            text: payload.to_string(),
            text_elements: Vec::new(),
        },
        UserInput::Image {
            image_url: "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==".to_string(),
            detail: None,
        },
    ];
    let canonical_body = json!({
        "agent_id": sender,
        "ref": stale_ref,
        "nickname": "SendTimeName",
        "task_path": "/root/send-time-task",
        "message": format!("{payload}\n[image]"),
    });
    let user_marker = format!(
        "<agent_message>\n{}\n</agent_message>",
        canonical_body
            .to_string()
            .replace('<', "\\u003c")
            .replace('>', "\\u003e")
    );
    let user_input = vec![UserInput::Text {
        text: user_marker.clone(),
        text_elements: Vec::new(),
    }];
    let accepted_agent = test
        .thread_store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: "fresh-agent-projection".to_string(),
            payload: MailboxPayload::Agent {
                input: original.clone(),
                attribution: Box::new(attribution.clone()),
            },
        })
        .await?;
    let accepted_user = test
        .thread_store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: "ordinary-user-markers".to_string(),
            payload: MailboxPayload::User {
                input: user_input.clone(),
                client_id: None,
            },
        })
        .await?;
    let mut first = server
        .mount_response(
            |_| true,
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("mail-projection"),
                    ev_function_call_with_namespace(
                        "consume-projection-mail",
                        "multi_agent_v1",
                        "check_mail",
                        "{}",
                    ),
                    ev_completed("mail-projection"),
                ]),
            }],
        )
        .await;
    let mut followup = server
        .mount_response(
            |_| true,
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("mail-projected"),
                    ev_assistant_message("mail-projected-done", "Mail reviewed."),
                    ev_completed("mail-projected"),
                ]),
            }],
        )
        .await;
    let submission = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Consume pending mail.".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let TurnInputSubmission::Started { turn_id } = submission else {
        anyhow::bail!("expected one receiver turn");
    };
    tokio::time::timeout(Duration::from_secs(/*secs*/ 15), first.wait_for_request()).await?;
    let request = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 15),
        followup.wait_for_request(),
    )
    .await?
    .body_json();
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    // Inspect the fixed claim only after production check_mail has consumed it.
    let claim = test
        .thread_store
        .claim_mailbox_input(ClaimMailboxInputParams {
            invocation: MailboxInvocation {
                receiver_thread_id: receiver,
                turn_id,
                tool_call_id: "consume-projection-mail".to_string(),
            },
            selection: MailboxSelection::All,
        })
        .await?;
    assert_eq!(
        claim
            .messages
            .iter()
            .map(|member| (&member.message.id, member.message.state))
            .collect::<Vec<_>>(),
        vec![
            (&accepted_agent.id, MailboxMessageState::Consumed),
            (&accepted_user.id, MailboxMessageState::Consumed),
        ],
    );
    let history = test
        .thread_store
        .load_rollback_history(LoadThreadHistoryParams {
            thread_id: receiver,
            include_archived: false,
        })
        .await?;
    for (index, member) in claim.messages.iter().enumerate() {
        let delivery_id = codex_protocol::mailbox_delivery_response_item_id(&member.delivery_id)
            .expect("delivery ID");
        let contexts = history
            .items
            .iter()
            .filter_map(|item| match item {
                RolloutItem::ResponseItem(item) if item.id() == Some(&delivery_id) => {
                    Some(&item.item)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let [
            ResponseItem::Message {
                role,
                content,
                internal_chat_message_metadata_passthrough: Some(metadata),
                ..
            },
        ] = contexts.as_slice()
        else {
            anyhow::bail!("expected one canonical annotated mailbox message");
        };
        assert_eq!(role, "user");
        let expected_kind = if index == 0 {
            "multi_agent.attributed_agent_message"
        } else {
            "user.text"
        };
        assert_eq!(
            metadata
                .content_item_kinds
                .as_ref()
                .and_then(|kinds| kinds.first()),
            Some(&ContentItemKind(expected_kind.to_string())),
        );
        let Some(ContentItem::InputText { text }) = content.first() else {
            anyhow::bail!("expected the canonical envelope first");
        };
        assert_eq!(text, &user_marker);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                text.strip_prefix("<agent_message>")
                    .and_then(|text| text.strip_suffix("</agent_message>"))
                    .expect("canonical envelope markers"),
            )?,
            canonical_body,
        );
        let model_messages = request["input"]
            .as_array()
            .expect("model input")
            .iter()
            .filter(|item| {
                item["type"] == "message"
                    && item["role"] == "user"
                    && item["content"][0]["text"]
                        .as_str()
                        .is_some_and(|text| text.starts_with("<agent_message>"))
                    && item["internal_chat_message_metadata_passthrough"]["content_item_kinds"][0]
                        == expected_kind
            })
            .collect::<Vec<_>>();
        assert_eq!(model_messages.len(), 1);
        let mut expected_content = serde_json::to_value(content)?;
        if index == 0 {
            let projected_text = model_messages[0]["content"][0]["text"]
                .as_str()
                .expect("projected envelope");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(
                    projected_text
                        .strip_prefix("<agent_message>")
                        .and_then(|text| text.strip_suffix("</agent_message>"))
                        .expect("projected envelope markers"),
                )?,
                json!({
                    "ref": receiver_ref.to_string(),
                    "message": format!("{payload}\n[image]"),
                }),
            );
            assert!(
                content
                    .iter()
                    .any(|item| matches!(item, ContentItem::InputImage { .. }))
            );
            expected_content[0]["text"] = json!(projected_text);
        }
        // Projection changes only trusted first-text content, never attachments or user text.
        assert_eq!(model_messages[0]["content"], expected_content);
        let expected_presentation = if index == 0 {
            let mut item = AgentMessageItem::new(&[]);
            item.id = delivery_id.to_string();
            item.phase = Some(MessagePhase::Commentary);
            item.attribution = Some(attribution.clone());
            item.input = Some(original.clone());
            TurnItem::AgentMessage(item)
        } else {
            TurnItem::UserMessage(UserMessageItem {
                id: delivery_id.to_string(),
                client_id: None,
                content: user_input.clone(),
            })
        };
        let presentations = history
            .items
            .iter()
            .filter_map(|item| match item {
                RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                    if event.item.id() == delivery_id.as_str() =>
                {
                    Some(&event.item)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            serde_json::to_value(presentations)?,
            serde_json::to_value(vec![expected_presentation])?,
        );
    }
    test.codex.shutdown_and_wait().await?;
    server.shutdown().await;
    Ok(())
}
