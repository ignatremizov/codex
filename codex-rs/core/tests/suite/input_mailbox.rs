//! Direct delivery ordering, structured authorship, and fixed-claim recovery.

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::config::ThreadStoreConfig;
use codex_features::Feature;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::AgentInputAttribution;
use codex_protocol::AgentInputIdentity;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::LocalImagePreparation;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::user_input::UserInput;
use codex_thread_store::AcceptMailboxInputParams;
use codex_thread_store::AppendThreadItemsParams;
use codex_thread_store::ClaimMailboxInputParams;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::MailboxInvocation;
use codex_thread_store::MailboxMessageState;
use codex_thread_store::MailboxPayload;
use codex_thread_store::MailboxSelection;
use codex_thread_store::MailboxSender;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use test_case::test_case;
use tokio::sync::oneshot;

#[path = "input_mailbox_permission_tests.rs"]
mod permission_tests;
#[path = "input_mailbox_projection_tests.rs"]
mod projection_tests;
#[path = "input_mailbox_selection_tests.rs"]
mod selection_tests;
#[path = "input_mailbox_wait_tests.rs"]
mod wait_tests;

#[derive(Clone, Copy)]
enum RecoveryBoundary {
    Fresh,
    Claimed,
    ContextOnly,
    PresentationOnly,
    Complete,
}

#[test_case(RecoveryBoundary::Fresh; "fresh direct consumption")]
#[test_case(RecoveryBoundary::Claimed; "fixed membership before append")]
#[test_case(RecoveryBoundary::ContextOnly; "reuse exact prepared context")]
#[test_case(RecoveryBoundary::PresentationOnly; "reuse exact presentation")]
#[test_case(RecoveryBoundary::Complete; "reconcile before acknowledgement")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_check_mail_recovers_fixed_user_batch_without_consuming_other_mail(
    boundary: RecoveryBoundary,
) -> Result<()> {
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let (release, gate) = oneshot::channel();
    let mut first = server
        .mount_response(
            |_| true,
            vec![StreamingSseChunk {
                gate: Some(gate),
                body: sse(vec![
                    ev_response_created("mail-call"),
                    ev_function_call_with_namespace(
                        "check-user-mail",
                        "multi_agent_v1",
                        "check_mail",
                        r#"{"from":"user"}"#,
                    ),
                    ev_completed("mail-call"),
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
                    ev_response_created("mail-followup"),
                    ev_assistant_message("mail-done", "Mail reviewed."),
                    ev_completed("mail-followup"),
                ]),
            }],
        )
        .await;
    let store_id = uuid::Uuid::now_v7().to_string();
    let configured_store_id = store_id.clone();
    let mut builder = test_codex()
        .with_history_mode(ThreadHistoryMode::Legacy)
        .with_config(move |config| {
            config.experimental_thread_store = ThreadStoreConfig::InMemory {
                id: configured_store_id,
            };
            config
                .features
                .enable(Feature::Collab)
                .expect("enable V1 tools");
        });
    let test = builder
        .build_with_streaming_server_auto_env(&server)
        .await?;
    let receiver = test.session_configured.thread_id;
    let input = vec![UserInput::Text {
        text: "Original user mailbox content.".to_string(),
        text_elements: Vec::new(),
    }];
    let accepted = test
        .thread_store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: "first-user-mail".to_string(),
            payload: MailboxPayload::User {
                input: input.clone(),
                client_id: Some("user-client-id".to_string()),
            },
        })
        .await?;
    let foreign = ThreadId::new();
    let identity = |thread_id| AgentInputIdentity {
        thread_id,
        nickname: Some("StoredSender".to_string()),
        agent_ref: None,
        task_path: None,
        role: None,
        model: None,
        reasoning_effort: None,
    };
    let unselected = test
        .thread_store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: "unselected-agent".to_string(),
            payload: MailboxPayload::Agent {
                input: vec![UserInput::Text {
                    text: "Unselected foreign mail must remain private.".to_string(),
                    text_elements: Vec::new(),
                }],
                attribution: Box::new(AgentInputAttribution {
                    sender: identity(foreign),
                    recipient: identity(receiver),
                    sender_turn_id: "stored-sender-turn".to_string(),
                }),
            },
        })
        .await?;
    let submission = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Check only user mail.".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let TurnInputSubmission::Started { turn_id } = submission else {
        anyhow::bail!("test must start one receiver turn");
    };
    tokio::time::timeout(Duration::from_secs(/*secs*/ 15), first.wait_for_request()).await?;
    let claim_params = ClaimMailboxInputParams {
        invocation: MailboxInvocation {
            receiver_thread_id: receiver,
            turn_id: turn_id.clone(),
            tool_call_id: "check-user-mail".to_string(),
        },
        selection: MailboxSelection::Senders(vec![MailboxSender::User]),
    };
    let mut prepared_before = None;
    let mut completion_before = None;
    let mut late = None;
    if !matches!(boundary, RecoveryBoundary::Fresh) {
        let claim = test
            .thread_store
            .claim_mailbox_input(claim_params.clone())
            .await?;
        assert_eq!(claim.messages.len(), 1);
        let id = codex_protocol::mailbox_delivery_response_item_id(&claim.messages[0].delivery_id)
            .expect("reserved delivery ID");
        let mut context = ResponseItem::from(ResponseInputItem::from_user_input(
            input.clone(),
            LocalImagePreparation::Defer,
        ));
        context.set_id(Some(id.clone()));
        context.set_turn_id_if_missing(&turn_id);
        // Distinct metadata makes an accidental re-preparation visible in deep equality.
        context.set_create_time_if_missing(serde_json::Number::from(42));
        let context = ResponseItemEnvelope::new(context);
        let completion = ItemCompletedEvent {
            thread_id: receiver,
            turn_id: turn_id.clone(),
            item: TurnItem::UserMessage(UserMessageItem {
                id: id.to_string(),
                client_id: Some("user-client-id".to_string()),
                content: input.clone(),
            }),
            started_at_ms: Some(41),
            completed_at_ms: 42,
        };
        let mut prior = Vec::new();
        if !matches!(boundary, RecoveryBoundary::Claimed) {
            let mut call: ResponseItem = serde_json::from_value(json!({
                "type": "function_call", "call_id": "check-user-mail",
                "namespace": "multi_agent_v1", "name": "check_mail", "arguments": "{\"from\":\"user\"}",
            }))?;
            call.set_id(Some(ResponseItemId::new("fc")));
            call.set_turn_id_if_missing(&turn_id);
            prior.push(RolloutItem::ResponseItem(call.into()));
            let mut result = ResponseItem::from(ResponseInputItem::FunctionCallOutput {
                call_id: "check-user-mail".to_string(),
                output: FunctionCallOutputPayload::from_text(
                    json!({"status": "delivery_requested", "from": "user"}).to_string(),
                ),
            });
            result.set_id(Some(ResponseItemId::new("fco")));
            result.set_turn_id_if_missing(&turn_id);
            prior.push(RolloutItem::ResponseItem(result.into()));
        }
        if matches!(
            boundary,
            RecoveryBoundary::ContextOnly | RecoveryBoundary::Complete
        ) {
            prior.push(RolloutItem::ResponseItem(context.clone()));
            prepared_before = Some(context);
        }
        if matches!(
            boundary,
            RecoveryBoundary::PresentationOnly | RecoveryBoundary::Complete
        ) {
            prior.push(RolloutItem::EventMsg(EventMsg::ItemCompleted(
                completion.clone(),
            )));
            completion_before = Some(completion);
        }
        if !prior.is_empty() {
            test.thread_store
                .append_items_and_flush(AppendThreadItemsParams {
                    thread_id: receiver,
                    items: prior,
                })
                .await?;
        }
        late = Some(
            test.thread_store
                .accept_mailbox_input(AcceptMailboxInputParams {
                    receiver_thread_id: receiver,
                    submission_key: "after-fixed-boundary".to_string(),
                    payload: MailboxPayload::User {
                        input: vec![UserInput::Text {
                            text: "Late mail must remain pending.".to_string(),
                            text_elements: Vec::new(),
                        }],
                        client_id: None,
                    },
                })
                .await?,
        );
    }
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("mail response gate closed"))?;
    let request = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 15),
        followup.wait_for_request(),
    )
    .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let request = request.body_json();
    let model_items = request["input"].as_array().expect("structured model input");
    let outputs = model_items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            item["type"] == "function_call_output" && item["call_id"] == "check-user-mail"
        })
        .collect::<Vec<_>>();
    assert_eq!(outputs.len(), 1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            outputs[0].1["output"].as_str().expect("metadata only")
        )?,
        json!({"status": "delivery_requested", "from": "user"}),
    );
    let user_content = model_items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            item["type"] == "message" && item.to_string().contains("Original user mailbox content.")
        })
        .collect::<Vec<_>>();
    assert_eq!(user_content.len(), 1);
    assert!(
        outputs[0].0 < user_content[0].0,
        "tool result must precede structured mail"
    );
    assert!(!request.to_string().contains("Unselected foreign mail"));
    assert!(
        !request
            .to_string()
            .contains("Late mail must remain pending")
    );

    let claimed = test.thread_store.claim_mailbox_input(claim_params).await?;
    assert_eq!(
        claimed
            .messages
            .iter()
            .map(|member| (&member.message.id, member.message.state))
            .collect::<Vec<_>>(),
        vec![(&accepted.id, MailboxMessageState::Consumed)],
    );
    let delivery_id =
        codex_protocol::mailbox_delivery_response_item_id(&claimed.messages[0].delivery_id)
            .expect("delivery ID");
    let history = test
        .thread_store
        .load_rollback_history(LoadThreadHistoryParams {
            thread_id: receiver,
            include_archived: false,
        })
        .await?;
    let contexts = history
        .items
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(item) if item.id() == Some(&delivery_id) => {
                Some(item.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(contexts.len(), 1);
    if let Some(prepared) = prepared_before {
        assert_eq!(contexts, vec![prepared]);
    }
    let presentations = history
        .items
        .iter()
        .filter_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                if event.item.id() == delivery_id.as_str() =>
            {
                Some(event)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(presentations.len(), 1);
    assert_eq!(
        serde_json::to_value(&presentations[0].item)?,
        serde_json::to_value(TurnItem::UserMessage(UserMessageItem {
            id: delivery_id.to_string(),
            client_id: Some("user-client-id".to_string()),
            content: input,
        }))?,
    );
    if let Some(completion) = completion_before {
        assert_eq!(
            serde_json::to_value(presentations[0])?,
            serde_json::to_value(completion)?
        );
    }
    let remaining = test
        .thread_store
        .claim_mailbox_input(ClaimMailboxInputParams {
            invocation: MailboxInvocation {
                receiver_thread_id: receiver,
                turn_id,
                tool_call_id: "inspect-unselected".to_string(),
            },
            selection: MailboxSelection::All,
        })
        .await?;
    let mut expected_remaining = vec![unselected.id];
    if let Some(late) = late {
        expected_remaining.push(late.id);
    }
    assert_eq!(
        remaining
            .messages
            .iter()
            .map(|member| member.message.id.clone())
            .collect::<Vec<_>>(),
        expected_remaining,
    );
    test.codex.shutdown_and_wait().await?;
    InMemoryThreadStore::remove_id(&store_id);
    server.shutdown().await;
    Ok(())
}
