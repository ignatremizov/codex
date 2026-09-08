use super::*;
use codex_core::config::Config;
use codex_protocol::protocol::MultiAgentVersion;
use codex_thread_store::AcceptMailboxInputParams;
use codex_thread_store::MailboxPayload;
use codex_thread_store::MailboxSender;
use codex_thread_store::MailboxSenderInventory;
use pretty_assertions::assert_eq;

#[derive(Default)]
struct InstalledInventory {
    extensions: OnceLock<ExtensionRegistry<Config>>,
}

impl ThreadLifecycleContributor<Config> for InstalledInventory {
    fn on_thread_idle<'a>(&'a self, input: ThreadIdleInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(extensions) = self.extensions.get() {
                for contributor in extensions.thread_lifecycle_contributors() {
                    contributor
                        .on_thread_idle(ThreadIdleInput {
                            cause: input.cause,
                            session_store: input.session_store,
                            thread_store: input.thread_store,
                        })
                        .await;
                }
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn obsolete_unrecorded_inventory_is_cancelled_without_a_wake() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let receiver = test.session_configured.thread_id;
    accept_user_mail(&test, "obsolete", "obsolete private mail").await?;
    let original = test
        .thread_store
        .prepare_mailbox_inventory(receiver)
        .await?
        .context("prepared inventory missing")?;
    let mail = test
        .thread_store
        .lookup_mailbox_input(receiver, "obsolete")
        .await?
        .context("accepted mail missing")?;
    test.thread_store
        .reject_mailbox_input(codex_thread_store::RejectMailboxInputParams {
            receiver_thread_id: receiver,
            message_id: mail.id,
            reason: "explicit test rejection before notification".to_string(),
        })
        .await?;
    test.codex
        .try_start_mailbox_inventory_if_idle_with_lease(())
        .await?;
    assert_eq!(
        test.thread_store.read_mailbox_inventory(receiver).await?,
        codex_thread_store::MailboxInventory {
            receiver_thread_id: receiver,
            notified_through: 0,
            pending_senders: Vec::new(),
            claimed_senders: Vec::new(),
            active_notification: None,
        },
    );
    assert!(
        test.thread_store
            .cancel_mailbox_inventory(original)
            .await
            .is_err()
    );
    assert!(
        server
            .received_requests()
            .await
            .context("captured HTTP requests")?
            .iter()
            .all(|request| request.url.path() != "/v1/responses")
    );
    Ok(())
}

async fn accept_user_mail(test: &TestCodex, key: &str, payload: &str) -> anyhow::Result<i64> {
    Ok(test
        .thread_store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: test.session_configured.thread_id,
            submission_key: key.to_string(),
            payload: MailboxPayload::User {
                input: vec![UserInput::Text {
                    text: payload.to_string(),
                    text_elements: Vec::new(),
                }],
                client_id: None,
            },
        })
        .await?
        .acceptance_sequence)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn newer_same_sender_mail_replaces_an_obsolete_unrecorded_frontier() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let captured =
        responses::mount_sse_once(&server, responses::sse_completed("new-frontier")).await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let receiver = test.session_configured.thread_id;
    accept_user_mail(&test, "old-frontier", "OLD_PRIVATE_PAYLOAD").await?;
    let old_snapshot = test
        .thread_store
        .prepare_mailbox_inventory(receiver)
        .await?
        .context("old inventory missing")?;
    let old = test
        .thread_store
        .lookup_mailbox_input(receiver, "old-frontier")
        .await?
        .context("old acceptance missing")?;
    test.thread_store
        .reject_mailbox_input(codex_thread_store::RejectMailboxInputParams {
            receiver_thread_id: receiver,
            message_id: old.id,
            reason: "old frontier retired".to_string(),
        })
        .await?;
    let new_sequence = accept_user_mail(&test, "new-frontier", "NEW_PRIVATE_PAYLOAD").await?;
    assert_eq!(
        test.codex
            .try_start_mailbox_inventory_if_idle_with_lease(())
            .await?,
        codex_core::MailboxInventoryAdmission::Started,
    );
    let turn_id = wait_for_event_match(test.codex.as_ref(), |event| {
        if let EventMsg::TurnComplete(event) = event {
            Some(event.turn_id.clone())
        } else {
            None
        }
    })
    .await;
    assert_ne!(turn_id, old_snapshot.id);
    let inventory = test.thread_store.read_mailbox_inventory(receiver).await?;
    assert_eq!(inventory.notified_through, new_sequence);
    assert_eq!(
        inventory.pending_senders,
        vec![MailboxSenderInventory {
            sender: MailboxSender::User,
            count: 1,
            max_acceptance_sequence: new_sequence,
        }]
    );
    let request = captured.single_request().body_json().to_string();
    assert!(!request.contains(&old_snapshot.id));
    assert!(!request.contains("OLD_PRIVATE_PAYLOAD"));
    assert!(!request.contains("NEW_PRIVATE_PAYLOAD"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_input_inventory_previews_without_pinning_and_records_before_sampling()
-> anyhow::Result<()> {
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let (release, gate) = oneshot::channel();
    let mut response = server
        .mount_response(
            |_| true,
            vec![StreamingSseChunk {
                gate: Some(gate),
                body: responses::sse_completed("first-input-inventory"),
            }],
        )
        .await;
    let test = test_codex()
        .build_with_streaming_server_auto_env(&server)
        .await?;
    let prior_version = test.codex.multi_agent_version();
    assert_eq!(
        test.codex.effective_mailbox_multi_agent_version().await,
        MultiAgentVersion::V1
    );
    assert_eq!(test.codex.multi_agent_version(), prior_version);
    let receiver = test.session_configured.thread_id;
    let sequence = accept_user_mail(&test, "first-input", "FIRST_INPUT_PRIVATE_MAIL").await?;
    // Preserve the actual preparation to check its UUID is the turn ID.
    let notification = test
        .thread_store
        .prepare_mailbox_inventory(receiver)
        .await?
        .context("inventory preparation missing")?;
    test.codex
        .try_start_mailbox_inventory_if_idle_with_lease(())
        .await?;
    let request = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 15),
        response.wait_for_request(),
    )
    .await?;
    assert!(!request.body_contains_text("FIRST_INPUT_PRIVATE_MAIL"));
    assert!(request.body_contains_text(&notification.id));
    // A request has reached the model but its response is still gated.
    let inventory = test.thread_store.read_mailbox_inventory(receiver).await?;
    assert_eq!(inventory.notified_through, sequence);
    assert_eq!(inventory.active_notification, None);
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("inventory response gate closed"))?;
    let completed = wait_for_event_match(test.codex.as_ref(), |event| {
        if let EventMsg::TurnComplete(completed) = event {
            Some(completed.clone())
        } else {
            None
        }
    })
    .await;
    assert_eq!(completed.turn_id, notification.id);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_user_runs_before_inventory_and_unread_mail_does_not_repeat() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let captured = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse_completed("queued-user"),
            responses::sse_completed("inventory"),
        ],
    )
    .await;
    let queue_slot = Arc::new(InstalledQueue::default());
    let inventory_slot = Arc::new(InstalledInventory::default());
    let mut registry = ExtensionRegistryBuilder::new();
    registry.thread_lifecycle_contributor(queue_slot.clone());
    registry.thread_lifecycle_contributor(inventory_slot.clone());
    let test = test_codex()
        .with_extensions(Arc::new(registry.build()))
        .build_with_auto_env(&server)
        .await?;
    let receiver = test.session_configured.thread_id;
    let sequence = accept_user_mail(&test, "mail", "PRIVATE_PAYLOAD_NOT_IN_INVENTORY").await?;
    // Stage without a loaded manager so installation, not enqueue, starts dispatch.
    let staging = QueuedItemService::new(
        loaded_thread_queue(&test)?,
        Weak::new(),
        Arc::new(NoopExtensionEventSink),
    );
    staging
        .enqueue(receiver, user_input("queued user first"))
        .await?;
    let queue = install_registered_queue(&test, &queue_slot)?;
    let mut fallback_registry = ExtensionRegistryBuilder::new();
    codex_queue_extension::install_inventory_fallback(&mut fallback_registry, queue.clone());
    assert!(
        inventory_slot
            .extensions
            .set(fallback_registry.build())
            .is_ok()
    );

    test.codex
        .emit_thread_idle_lifecycle_if_idle(ThreadIdleCause::Completed)
        .await;
    for _ in 0..2 {
        wait_for_event_match(test.codex.as_ref(), |event| {
            matches!(event, EventMsg::TurnComplete(_)).then_some(())
        })
        .await;
    }
    // Each reevaluation is awaited; the durable frontier, not a sleep, suppresses wake.
    for _ in 0..3 {
        test.codex
            .emit_thread_idle_lifecycle_if_idle(ThreadIdleCause::Completed)
            .await;
    }
    let requests = captured.requests();
    assert_eq!(
        test.codex.multi_agent_version(),
        Some(MultiAgentVersion::V1)
    );
    assert_eq!(requests.len(), 2);
    assert_eq!(
        server
            .received_requests()
            .await
            .context("captured HTTP requests")?
            .iter()
            .filter(|request| request.url.path() == "/v1/responses")
            .count(),
        2,
    );
    assert_eq!(
        requests[0]
            .message_input_texts("user")
            .last()
            .map(String::as_str),
        Some("queued user first")
    );
    assert!(
        !requests[0]
            .body_json()
            .to_string()
            .contains("Mailbox inventory")
    );
    assert!(
        requests[1]
            .body_json()
            .to_string()
            .contains("Mailbox inventory")
    );
    for request in &requests {
        assert!(
            !request
                .body_json()
                .to_string()
                .contains("PRIVATE_PAYLOAD_NOT_IN_INVENTORY")
        );
    }
    let inventory = test.thread_store.read_mailbox_inventory(receiver).await?;
    assert_eq!(
        inventory,
        codex_thread_store::MailboxInventory {
            receiver_thread_id: receiver,
            notified_through: sequence,
            pending_senders: vec![MailboxSenderInventory {
                sender: MailboxSender::User,
                count: 1,
                max_acceptance_sequence: sequence,
            }],
            claimed_senders: Vec::new(),
            active_notification: None,
        }
    );
    assert!(queue.list(receiver).await?.is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_turn_defers_inventory_and_new_arrival_advances_frontier() -> anyhow::Result<()> {
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let (release, gate) = oneshot::channel();
    let mut active = server
        .mount_response(
            |_| true,
            vec![StreamingSseChunk {
                gate: Some(gate),
                body: responses::sse_completed("active"),
            }],
        )
        .await;
    let test = test_codex()
        .build_with_streaming_server_auto_env(&server)
        .await?;
    let receiver = test.session_configured.thread_id;
    // Admission must return before the gated response completes.
    assert!(matches!(
        test.codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "keep working".to_string(),
                text_elements: Vec::new(),
            }]))
            .await?,
        TurnInputSubmission::Started { .. }
    ));
    tokio::time::timeout(Duration::from_secs(/*secs*/ 15), active.wait_for_request()).await?;
    let first_sequence = accept_user_mail(&test, "first", "FIRST_PRIVATE_MAIL").await?;
    test.codex
        .try_start_mailbox_inventory_if_idle_with_lease(())
        .await?;
    let before = test.thread_store.read_mailbox_inventory(receiver).await?;
    assert_eq!(before.notified_through, 0);
    assert_eq!(before.active_notification, None);
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("active response gate closed"))?;
    wait_for_event_match(test.codex.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_)).then_some(())
    })
    .await;
    for (response_id, expected_count) in [("first-inventory", 1), ("new-inventory", 2)] {
        let expected_sequence = if expected_count == 1 {
            first_sequence
        } else {
            accept_user_mail(&test, "second", "SECOND_PRIVATE_MAIL").await?
        };
        let mut response = server
            .mount_response(
                |_| true,
                vec![StreamingSseChunk {
                    gate: None,
                    body: responses::sse_completed(response_id),
                }],
            )
            .await;
        test.codex
            .try_start_mailbox_inventory_if_idle_with_lease(())
            .await?;
        let request = tokio::time::timeout(
            Duration::from_secs(/*secs*/ 15),
            response.wait_for_request(),
        )
        .await?;
        assert!(!request.body_contains_text("FIRST_PRIVATE_MAIL"));
        assert!(!request.body_contains_text("SECOND_PRIVATE_MAIL"));
        wait_for_event_match(test.codex.as_ref(), |event| {
            matches!(event, EventMsg::TurnComplete(_)).then_some(())
        })
        .await;
        let inventory = test.thread_store.read_mailbox_inventory(receiver).await?;
        assert_eq!(inventory.notified_through, expected_sequence);
        assert_eq!(
            inventory.pending_senders,
            vec![MailboxSenderInventory {
                sender: MailboxSender::User,
                count: expected_count,
                max_acceptance_sequence: expected_sequence,
            }]
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_inventory_only_thread_wakes_once_without_queue_revision() -> anyhow::Result<()> {
    assert_inventory_resume(InventoryResumeBoundary::Unrecorded).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recorded_inventory_resume_repairs_ack_without_restarting_original_turn()
-> anyhow::Result<()> {
    assert_inventory_resume(InventoryResumeBoundary::Recorded).await
}

enum InventoryResumeBoundary {
    Unrecorded,
    Recorded,
}

async fn assert_inventory_resume(boundary: InventoryResumeBoundary) -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let mut bodies = vec![responses::sse_completed("initial-user")];
    if matches!(boundary, InventoryResumeBoundary::Unrecorded) {
        bodies.push(responses::sse_completed("resumed-inventory"));
    }
    let expected_requests = bodies.len();
    let captured = responses::mount_sse_sequence(&server, bodies).await;
    let queue_slot = Arc::new(InstalledQueue::default());
    let inventory_slot = Arc::new(InstalledInventory::default());
    let mut registry = ExtensionRegistryBuilder::new();
    registry.thread_lifecycle_contributor(queue_slot.clone());
    registry.thread_lifecycle_contributor(inventory_slot.clone());
    let test = test_codex()
        .with_extensions(Arc::new(registry.build()))
        .build_with_auto_env(&server)
        .await?;
    let receiver = test.session_configured.thread_id;
    // This helper already waits for and consumes the matching TurnComplete.
    test.submit_text_turn("establish receiver history").await?;
    let rollout = test
        .codex
        .rollout_path()
        .context("receiver rollout missing")?;
    let sequence = accept_user_mail(&test, "pending-on-resume", "UNLOADED_PRIVATE_MAIL").await?;
    if matches!(boundary, InventoryResumeBoundary::Recorded) {
        let notification = test
            .thread_store
            .prepare_mailbox_inventory(receiver)
            .await?
            .context("prepared inventory missing")?;
        let context = notification.context()?;
        // Persist the real rollout wire envelope, simulating a process ending
        // after its canonical append and before the separate SQL acknowledgement.
        test.thread_store
            .append_items_and_flush(codex_thread_store::AppendThreadItemsParams {
                thread_id: receiver,
                items: vec![serde_json::from_value(serde_json::json!({
                    "type": "response_item",
                    "payload": context.item,
                    "metadata": context.metadata,
                }))?],
            })
            .await?;
        assert!(matches!(
            test.thread_store
                .recover_mailbox_inventory(notification)
                .await?,
            codex_thread_store::MailboxInventoryRecovery::Recorded { .. },
        ));
    }
    test.codex.shutdown_and_wait().await?;
    test.thread_manager.remove_thread(&receiver).await;
    assert_eq!(captured.requests().len(), 1);
    assert_eq!(
        test.thread_store
            .read_mailbox_inventory(receiver)
            .await?
            .notified_through,
        0,
    );
    // TurnComplete precedes the original runtime's idle callbacks. Keep both
    // proxies uninstalled until it has stopped so those callbacks cannot deliver
    // the pending inventory before the resume boundary this test exercises.
    let queue = install_registered_queue(&test, &queue_slot)?;
    let mut fallback_registry = ExtensionRegistryBuilder::new();
    codex_queue_extension::install_inventory_fallback(&mut fallback_registry, queue.clone());
    assert!(
        inventory_slot
            .extensions
            .set(fallback_registry.build())
            .is_ok()
    );
    // Start the real watcher before resume. The registered proxy forwards real
    // resume callbacks to this service; no manual idle callback is used here.
    let mut watcher_registry = ExtensionRegistryBuilder::<Config>::new();
    codex_queue_extension::install(&mut watcher_registry, queue.clone());
    assert!(queue.list(receiver).await?.is_empty());
    let resumed = test
        .thread_manager
        .resume_thread_from_rollout(
            test.config.clone(),
            rollout.clone(),
            test.thread_manager.auth_manager(),
            /*parent_trace*/ None,
            Default::default(),
        )
        .await?;
    match boundary {
        InventoryResumeBoundary::Unrecorded => {
            wait_for_event_with_timeout(
                resumed.thread.as_ref(),
                |event| matches!(event, EventMsg::TurnComplete(_)),
                Duration::from_secs(/*secs*/ 25),
            )
            .await;
        }
        InventoryResumeBoundary::Recorded => {
            tokio::time::timeout(Duration::from_secs(/*secs*/ 25), async {
                loop {
                    if test
                        .thread_store
                        .read_mailbox_inventory(receiver)
                        .await?
                        .notified_through
                        == sequence
                    {
                        return anyhow::Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(/*millis*/ 100)).await;
                }
            })
            .await??;
        }
    }
    let acknowledged = test.thread_store.read_mailbox_inventory(receiver).await?;
    assert_eq!(acknowledged.notified_through, sequence);
    assert_eq!(captured.requests().len(), expected_requests);
    for request in captured.requests() {
        assert!(
            !request
                .body_json()
                .to_string()
                .contains("UNLOADED_PRIVATE_MAIL")
        );
    }
    resumed.thread.shutdown_and_wait().await?;
    test.thread_manager.remove_thread(&receiver).await;
    let _resumed_again = test
        .thread_manager
        .resume_thread_from_rollout(
            test.config.clone(),
            rollout,
            test.thread_manager.auth_manager(),
            /*parent_trace*/ None,
            Default::default(),
        )
        .await?;
    // Cross two watcher ticks: a load gets one evaluation, not periodic empty
    // queue goal/inventory retries, and unchanged unread mail cannot wake again.
    tokio::time::sleep(Duration::from_secs(/*secs*/ 21)).await;
    assert_eq!(captured.requests().len(), expected_requests);
    assert_eq!(
        server
            .received_requests()
            .await
            .context("captured HTTP requests")?
            .iter()
            .filter(|request| request.url.path() == "/v1/responses")
            .count(),
        expected_requests,
    );
    assert_eq!(
        test.thread_store.read_mailbox_inventory(receiver).await?,
        acknowledged
    );
    Ok(())
}
