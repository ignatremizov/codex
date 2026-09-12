//! Inventory presentation uses advertised refs without changing durable wake progress.

use super::*;
use codex_core::MailboxInventoryAdmission;
use codex_core::UserAgentReplyRouteMode;
use codex_core::UserAgentResponseHandling;
use codex_core::UserAgentSpawnOptions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::Op;
use core_test_support::responses::mount_compact_json_once;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::start_mock_server;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use test_case::test_case;

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inventory_projects_frozen_sender_refs_and_restart_does_not_renotify(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    const HEADING: &str = "Pending mail snapshot (not consumed):\n";
    let server = start_mock_server().await;
    let configure = |config: &mut codex_core::config::Config| {
        config.features.enable(Feature::Collab).expect("enable V1");
        config
            .features
            .disable(Feature::MultiAgentV2)
            .expect("disable V2");
    };
    let mut builder = test_codex()
        .with_history_mode(history_mode)
        .with_config(configure);
    let test = builder.build_with_auto_env(&server).await?;
    let receiver = test.session_configured.thread_id;
    let spawned = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?;
    let sender = spawned.target_thread_id;
    let sender_ref = spawned.agent_ref.expect("advertised child ref").to_string();
    test.codex
        .set_agent_reply_route(
            &sender.to_string(),
            /*recipient*/ None,
            UserAgentReplyRouteMode::Enabled,
        )
        .await?;
    let foreign = ThreadId::new();
    let identity = |thread_id| AgentInputIdentity {
        thread_id,
        nickname: None,
        agent_ref: Some("999".to_string()),
        task_path: None,
        role: None,
        model: None,
        reasoning_effort: None,
    };
    for author in [sender, foreign] {
        test.thread_store
            .accept_mailbox_input(AcceptMailboxInputParams {
                receiver_thread_id: receiver,
                submission_key: format!("inventory-agent-{author}"),
                payload: MailboxPayload::Agent {
                    input: vec![UserInput::Text {
                        text: format!("Private mail from {author}."),
                        text_elements: Vec::new(),
                    }],
                    attribution: Box::new(AgentInputAttribution {
                        sender: identity(author),
                        recipient: identity(receiver),
                        sender_turn_id: "author-turn".to_string(),
                    }),
                },
            })
            .await?;
    }
    test.thread_store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: "inventory-user".to_string(),
            payload: MailboxPayload::User {
                input: vec![UserInput::Text {
                    text: "Original private user mail.".to_string(),
                    text_elements: Vec::new(),
                }],
                client_id: None,
            },
        })
        .await?;
    let frozen = test
        .thread_store
        .prepare_mailbox_inventory(receiver)
        .await?
        .expect("pending inventory");
    let canonical = frozen.context()?;
    let first = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("inventory"),
            ev_assistant_message("inventory-done", "I will select mail later."),
            ev_completed("inventory"),
        ]),
    )
    .await;
    assert_eq!(
        test.codex
            .try_start_mailbox_inventory_if_idle_with_lease(())
            .await?,
        MailboxInventoryAdmission::Started,
    );
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let request = first.single_request();
    let inventories = request
        .message_input_texts("developer")
        .into_iter()
        .filter(|text| text.starts_with(HEADING))
        .collect::<Vec<_>>();
    assert_eq!(inventories.len(), 1);
    let inventory_text = inventories[0].clone();
    let (body, guidance) = inventory_text
        .strip_prefix(HEADING)
        .expect("inventory heading")
        .split_once('\n')
        .expect("inventory guidance");
    let rows = frozen
        .pending_senders
        .iter()
        .map(|group| {
            let from = match group.sender {
                MailboxSender::Agent(id) if id == sender => sender_ref.clone(),
                MailboxSender::Agent(id) => id.to_string(),
                MailboxSender::User => "user".to_string(),
            };
            json!({"from": from, "count": group.count})
        })
        .collect::<Vec<_>>();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body)?,
        json!({"receiver": "1", "pending_senders": rows}),
    );
    assert_eq!(guidance, "check_mail: {\"from\":\"<ref>\"} or {} for all.");
    let hydration = request
        .message_input_texts("developer")
        .into_iter()
        .find(|text| text.starts_with("<agent_identity_context>"))
        .expect("advertised identity mapping");
    assert!(hydration.contains(&format!("\"ref\":\"{sender_ref}\"")));
    assert!(
        !request
            .body_json()
            .to_string()
            .contains("Private mail from")
    );
    assert!(
        !request
            .body_json()
            .to_string()
            .contains("Original private user mail.")
    );
    let history = test
        .thread_store
        .load_rollback_history(LoadThreadHistoryParams {
            thread_id: receiver,
            include_archived: false,
        })
        .await?;
    let recorded = history
        .items
        .into_iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(item) if item.id() == canonical.id() => Some(item),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(recorded, vec![canonical.clone()]);
    let inventory = test.thread_store.read_mailbox_inventory(receiver).await?;
    assert_eq!(inventory.pending_senders, frozen.pending_senders);
    assert_eq!(inventory.notified_through, frozen.through_sequence);
    assert_eq!(
        test.codex
            .try_start_mailbox_inventory_if_idle_with_lease(())
            .await?,
        MailboxInventoryAdmission::Deferred,
    );
    let home = Arc::clone(&test.home);
    let rollout = test.codex.rollout_path().expect("durable root rollout");
    test.thread_manager
        .get_thread(sender)
        .await?
        .shutdown_and_wait()
        .await?;
    test.codex.shutdown_and_wait().await?;
    drop(test);
    let resumed = builder
        .with_config(configure)
        .resume(&server, home, rollout)
        .await?;
    assert_eq!(
        resumed
            .codex
            .try_start_mailbox_inventory_if_idle_with_lease(())
            .await?,
        MailboxInventoryAdmission::Deferred,
    );
    assert_eq!(
        resumed
            .thread_store
            .read_mailbox_inventory(receiver)
            .await?,
        inventory,
    );
    assert_eq!(first.requests().len(), 1);
    // Later arrivals do not rewrite the historical count, but are included when
    // check_mail is actually invoked. Both compact selector forms are usable.
    resumed
        .thread_store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: "later-user-mail".to_string(),
            payload: MailboxPayload::User {
                input: vec![UserInput::Text {
                    text: "Later private user mail.".to_string(),
                    text_elements: Vec::new(),
                }],
                client_id: None,
            },
        })
        .await?;
    let consumption = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("select-mail"),
                ev_function_call_with_namespace(
                    "select-agent",
                    "multi_agent_v1",
                    "check_mail",
                    &json!({"from": sender_ref}).to_string(),
                ),
                ev_function_call_with_namespace(
                    "select-user",
                    "multi_agent_v1",
                    "check_mail",
                    r#"{"from":"user"}"#,
                ),
                ev_completed("select-mail"),
            ]),
            sse(vec![
                ev_response_created("mail-delivered"),
                ev_assistant_message("mail-done", "Selected mail received."),
                ev_completed("mail-delivered"),
            ]),
        ],
    )
    .await;
    resumed
        .submit_turn("Consume the listed agent and user mail.")
        .await?;
    let requests = consumption.requests();
    assert_eq!(requests.len(), 2);
    let selection = &requests[0];
    assert!(
        selection
            .message_input_texts("developer")
            .contains(&inventory_text)
    );
    let receipt = &requests[1];
    for (call, from, count) in [
        ("select-agent", sender_ref.as_str(), 1),
        ("select-user", "user", 2),
    ] {
        let output = receipt.function_call_output(call);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(output["output"].as_str().expect("receipt"))?,
            json!({"from": from, "status": "delivered", "delivered_count": count, "rejected_count": 0}),
        );
    }
    let ResponseItem::Message { content, .. } = &canonical.item else {
        panic!("canonical inventory message");
    };
    assert!(matches!(&content[0], ContentItem::InputText { text }
        if text.contains("notification_id") && text.contains("max_acceptance_sequence")));
    resumed.codex.shutdown_and_wait().await?;
    Ok(())
}

#[derive(Clone, Copy)]
enum CompactionPath {
    Local,
    Remote,
    LegacyRemote,
}

#[test_case(ThreadHistoryMode::Legacy, CompactionPath::Local; "legacy local")]
#[test_case(ThreadHistoryMode::Paginated, CompactionPath::Local; "paginated local")]
#[test_case(ThreadHistoryMode::Legacy, CompactionPath::Remote; "legacy remote")]
#[test_case(ThreadHistoryMode::Paginated, CompactionPath::Remote; "paginated remote")]
#[test_case(ThreadHistoryMode::Legacy, CompactionPath::LegacyRemote; "legacy old remote")]
#[test_case(ThreadHistoryMode::Paginated, CompactionPath::LegacyRemote; "paginated old remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compaction_projects_inventory_except_legacy_remote(
    history_mode: ThreadHistoryMode,
    boundary: CompactionPath,
) -> Result<()> {
    let server = start_mock_server().await;
    let test = test_codex()
        .with_history_mode(history_mode)
        .with_auth(codex_login::CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(move |config| {
            config.features.enable(Feature::Collab).expect("enable V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("disable V2");
            match boundary {
                CompactionPath::Local => {
                    config
                        .features
                        .disable(Feature::RemoteCompaction)
                        .expect("local compaction");
                }
                CompactionPath::Remote => {
                    config
                        .features
                        .enable(Feature::RemoteCompaction)
                        .expect("remote compaction");
                    config
                        .features
                        .enable(Feature::RemoteCompactionV2)
                        .expect("new remote compaction");
                }
                CompactionPath::LegacyRemote => {
                    config
                        .features
                        .enable(Feature::RemoteCompaction)
                        .expect("remote compaction");
                    config
                        .features
                        .disable(Feature::RemoteCompactionV2)
                        .expect("legacy remote compaction");
                }
            }
        })
        .build_with_auto_env(&server)
        .await?;
    let receiver = test.session_configured.thread_id;
    test.thread_store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: "compact-inventory".to_string(),
            payload: MailboxPayload::User {
                input: vec![UserInput::Text {
                    text: "Mail remains private during compaction.".to_string(),
                    text_elements: Vec::new(),
                }],
                client_id: None,
            },
        })
        .await?;
    let frozen = test
        .thread_store
        .prepare_mailbox_inventory(receiver)
        .await?
        .expect("inventory");
    let canonical = frozen.context()?;
    let initial = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("inventory-visible", "Select mail later."),
            ev_completed("initial"),
        ]),
    )
    .await;
    assert_eq!(
        test.codex
            .try_start_mailbox_inventory_if_idle_with_lease(())
            .await?,
        MailboxInventoryAdmission::Started,
    );
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let initial = initial.single_request();
    let projected = initial
        .message_input_texts("developer")
        .into_iter()
        .find(|text| text.starts_with("Pending mail snapshot (not consumed):\n"))
        .expect("projected inventory");
    let json_line = projected.lines().nth(1).expect("inventory JSON");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(json_line)?,
        json!({"receiver": receiver.to_string(), "pending_senders": [{"from": "user", "count": 1}]}),
    );
    let compact = match boundary {
        CompactionPath::Local => {
            mount_sse_once(
                &server,
                sse(vec![
                    ev_assistant_message("summary", "Inventory remains pending."),
                    ev_completed("local-compact"),
                ]),
            )
            .await
        }
        CompactionPath::Remote => {
            mount_sse_once(
                &server,
                sse(vec![
                    json!({"type": "response.output_item.done", "item": {
                        "type": "compaction", "encrypted_content": "inventory-summary"
                    }}),
                    ev_completed("remote-compact"),
                ]),
            )
            .await
        }
        CompactionPath::LegacyRemote => {
            mount_compact_json_once(
                &server,
                json!({
                    "output": [{"type": "compaction", "encrypted_content": "inventory-summary"}]
                }),
            )
            .await
        }
    };
    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let request = compact.single_request();
    let actual = request
        .message_input_texts("developer")
        .into_iter()
        .filter(|text| {
            text.starts_with("Pending mail snapshot (not consumed):\n")
                || text.starts_with("Mailbox inventory (pending mail, not consumed):\n")
        })
        .collect::<Vec<_>>();
    let expected = match boundary {
        CompactionPath::Local | CompactionPath::Remote => projected,
        CompactionPath::LegacyRemote => {
            let ResponseItem::Message { content, .. } = &canonical.item else {
                panic!("inventory message");
            };
            let [ContentItem::InputText { text }] = content.as_slice() else {
                panic!("inventory text");
            };
            text.clone()
        }
    };
    assert_eq!(actual, vec![expected]);
    assert_eq!(
        test.thread_store
            .recover_mailbox_inventory(frozen.clone())
            .await?,
        codex_thread_store::MailboxInventoryRecovery::AlreadyCovered {
            context: canonical,
            notified_through: frozen.through_sequence,
        },
    );
    let inventory = test.thread_store.read_mailbox_inventory(receiver).await?;
    assert_eq!(inventory.pending_senders, frozen.pending_senders);
    assert_eq!(inventory.notified_through, frozen.through_sequence);
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
