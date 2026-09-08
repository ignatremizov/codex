use super::*;
use codex_core::UserAgentResponseHandling;
use codex_core::UserAgentSpawnOptions;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::start_mock_server;

#[derive(Clone, Copy)]
enum SenderSelector {
    ClosedRef,
    ClosedTask,
    ForeignUuid,
    ForeignForcedUuid,
}

#[test_case(SenderSelector::ClosedRef; "closed durable ref")]
#[test_case(SenderSelector::ClosedTask; "closed durable task")]
#[test_case(SenderSelector::ForeignUuid; "foreign UUID without lifecycle ownership")]
#[test_case(SenderSelector::ForeignForcedUuid; "foreign forced UUID")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_mail_resolves_sender_without_loading_or_adopting(
    selector: SenderSelector,
) -> Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config.features.enable(Feature::Collab).expect("enable V1");
        config
            .features
            .disable(Feature::MultiAgentV2)
            .expect("disable V2");
    });
    let test = builder.build_with_auto_env(&server).await?;
    let (sender, selector) = match selector {
        SenderSelector::ClosedRef | SenderSelector::ClosedTask => {
            let child = test
                .codex
                .spawn_agent(UserAgentSpawnOptions {
                    task: Some("mail-review".to_string()),
                    response_handling: UserAgentResponseHandling::Presentation,
                    ..Default::default()
                })
                .await?;
            test.codex
                .close_agent(
                    &child.target_thread_id.to_string(),
                    UserAgentResponseHandling::Presentation,
                )
                .await?;
            let selected = match selector {
                SenderSelector::ClosedRef => child.agent_ref.expect("durable ref").to_string(),
                SenderSelector::ClosedTask => child.task_path.expect("durable task path"),
                SenderSelector::ForeignUuid | SenderSelector::ForeignForcedUuid => unreachable!(),
            };
            (child.target_thread_id, selected)
        }
        SenderSelector::ForeignUuid => {
            let sender = ThreadId::new();
            (sender, sender.to_string())
        }
        SenderSelector::ForeignForcedUuid => {
            let sender = ThreadId::new();
            (sender, format!("id:{sender}"))
        }
    };
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("sender-selection"),
                ev_function_call_with_namespace(
                    "select-sender",
                    "multi_agent_v1",
                    "check_mail",
                    &json!({"from": selector}).to_string(),
                ),
                ev_completed("sender-selection"),
            ]),
            sse(vec![
                ev_response_created("selected"),
                ev_assistant_message("selected-done", "No selected mail."),
                ev_completed("selected"),
            ]),
        ],
    )
    .await;
    test.submit_turn("Check the selected sender.").await?;
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let output = requests[1].function_call_output("select-sender");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            output["output"].as_str().expect("metadata result")
        )?,
        json!({"status": "delivery_requested", "from": sender.to_string()}),
    );
    assert!(
        test.thread_manager.get_thread(sender).await.is_err(),
        "selection must not load the sender"
    );
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
