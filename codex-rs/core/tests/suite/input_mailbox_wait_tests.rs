use super::*;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::start_mock_server;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_wait_selects_foreign_mail_without_observing_status_or_consuming_user_mail()
-> Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config.features.enable(Feature::Collab).expect("enable V1");
        config
            .features
            .disable(Feature::MultiAgentV2)
            .expect("disable V2");
    });
    let test = builder.build_with_auto_env(&server).await?;
    let receiver = test.session_configured.thread_id;
    let sender = ThreadId::new();
    let identity = |thread_id| AgentInputIdentity {
        thread_id,
        nickname: None,
        agent_ref: None,
        task_path: None,
        role: None,
        model: None,
        reasoning_effort: None,
    };
    test.thread_store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: "selected-wait-mail".to_string(),
            payload: MailboxPayload::Agent {
                input: vec![UserInput::Text {
                    text: "Foreign mail still requires delivery permission.".to_string(),
                    text_elements: Vec::new(),
                }],
                attribution: Box::new(AgentInputAttribution {
                    sender: identity(sender),
                    recipient: identity(receiver),
                    sender_turn_id: "foreign-turn".to_string(),
                }),
            },
        })
        .await?;
    test.thread_store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: "unselected-wait-user".to_string(),
            payload: MailboxPayload::User {
                input: vec![UserInput::Text {
                    text: "User mail must remain pending.".to_string(),
                    text_elements: Vec::new(),
                }],
                client_id: None,
            },
        })
        .await?;
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("targeted-wait"),
                ev_function_call_with_namespace(
                    "wait-for-mail",
                    "multi_agent_v1",
                    "wait_agent",
                    &json!({"targets": [sender.to_string(), format!("id:{sender}")]}).to_string(),
                ),
                ev_completed("targeted-wait"),
            ]),
            sse(vec![
                ev_response_created("wait-returned"),
                ev_assistant_message("wait-done", "Wait returned."),
                ev_completed("wait-returned"),
            ]),
        ],
    )
    .await;
    test.submit_turn("Wait for the selected sender.").await?;
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let output = requests[1].function_call_output("wait-for-mail");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(output["output"].as_str().expect("wait result"),)?,
        json!({
            "status": {},
            "timed_out": false,
            "return_reason": "mail",
            "mail_only_targets": [sender.to_string()],
        }),
    );
    let inventory = test.thread_store.read_mailbox_inventory(receiver).await?;
    assert_eq!(
        inventory
            .pending_senders
            .into_iter()
            .map(|entry| (entry.sender, entry.count))
            .collect::<Vec<_>>(),
        vec![(MailboxSender::User, 1)],
    );
    assert!(inventory.claimed_senders.is_empty());
    assert!(test.thread_manager.get_thread(sender).await.is_err());
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
