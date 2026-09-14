use super::*;
use crate::UserAgentResponseHandling;
use crate::UserAgentSpawnOptions;
use codex_history::RolloutItem;
use codex_thread_store::AppendThreadItemsParams;
use codex_thread_store::MailboxFinalSubscriptionRequest;
use codex_thread_store::MailboxFinalSubscriptionState;
use codex_thread_store::MailboxPayload;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn mailbox_activity_hint_follows_persistence_without_delivering_payload() {
    const PAYLOAD: &str = "never-inject-mailbox-payload-activity-test";
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::Collab).expect("V1");
    config
        .features
        .disable(Feature::MultiAgentV2)
        .expect("not V2");
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (root_id, root) = harness.start_thread().await;
    let receiver = root
        .spawn_agent(UserAgentSpawnOptions::default())
        .await
        .expect("receiver")
        .target_thread_id;
    let target = harness
        .manager
        .get_thread(receiver)
        .await
        .expect("loaded receiver");
    let activity = target.session.subscribe_mailbox_activity();
    let control = root.session.services.agent_control.clone();
    let sender = root.session.presentation_id();
    assert!(root.session.begin_agent_response_turn("activity-origin"));
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    *control
        .wait_agent_presentations
        .mailbox_acceptance_gate
        .lock()
        .expect("mailbox gate") = Some((reached_tx, release_rx));
    let accepting_control = control.clone();
    let acceptance = tokio::spawn(async move {
        accepting_control
            .accept_mailbox_agent_input(
                sender,
                "activity-origin",
                "activity-call",
                receiver,
                text_input(PAYLOAD),
                MailboxFinalSubscriptionRequest::None,
            )
            .await
    });
    timeout(Duration::from_secs(/*secs*/ 15), reached_rx)
        .await
        .expect("pre-persist boundary")
        .expect("mailbox gate");
    assert!(!activity.has_changed().expect("activity channel"));
    release_tx.send(()).expect("release persistence");
    let accepted = timeout(Duration::from_secs(/*secs*/ 15), acceptance)
        .await
        .expect("acceptance completes")
        .expect("acceptance task")
        .expect("accepted");
    assert!(activity.has_changed().expect("post-commit activity hint"));
    assert_eq!(
        root.session
            .services
            .thread_store
            .lookup_mailbox_input(
                receiver,
                &serde_json::to_string(&(
                    "v1-send-input-mailbox",
                    root_id,
                    "activity-origin",
                    "activity-call"
                ))
                .expect("submission key"),
            )
            .await
            .expect("durable acceptance"),
        Some(accepted),
    );
    let after = target.session.clone_history().await;
    assert!(
        !serde_json::to_string(&after.raw_items().collect::<Vec<_>>())
            .expect("receiver context")
            .contains(PAYLOAD),
        "an inventory wake is allowed, but accepted payload remains outside model context",
    );
    root.close_agent(
        &receiver.to_string(),
        UserAgentResponseHandling::Presentation,
    )
    .await
    .expect("close receiver");
    root.shutdown_and_wait().await.expect("shutdown root");
}

#[tokio::test]
async fn accepted_bound_mailbox_retry_rebinds_the_actual_receiver_endpoint() {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::Collab).expect("V1");
    config
        .features
        .disable(Feature::MultiAgentV2)
        .expect("not V2");
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (root_id, root) = harness.start_thread().await;
    let receiver = root
        .spawn_agent(UserAgentSpawnOptions::default())
        .await
        .expect("receiver")
        .target_thread_id;
    let receiver_thread = harness
        .manager
        .get_thread(receiver)
        .await
        .expect("loaded receiver");
    let control = root.session.services.agent_control.clone();
    let sender = root.session.presentation_id();
    let call_id = "bound-mailbox-retry";
    let sender_turn_id = "bound-mailbox-origin";
    assert!(root.session.begin_agent_response_turn(sender_turn_id));
    let accepted = control
        .accept_mailbox_agent_input(
            sender,
            sender_turn_id,
            call_id,
            receiver,
            text_input("bind to the exact inventory turn"),
            MailboxFinalSubscriptionRequest::Wake,
        )
        .await
        .expect("mailbox acceptance");
    let submission_key =
        serde_json::to_string(&("v1-send-input-mailbox", root_id, sender_turn_id, call_id))
            .expect("mailbox idempotency key");
    let subscription_id = accepted
        .final_subscription
        .as_ref()
        .expect("conditional subscription")
        .message_id
        .clone();

    let store = &root.session.services.thread_store;
    let notification = store
        .prepare_mailbox_inventory(receiver)
        .await
        .expect("prepare inventory")
        .expect("pending inventory");
    store
        .append_items_and_flush(AppendThreadItemsParams {
            thread_id: receiver,
            items: vec![RolloutItem::ResponseItem(
                notification.context().expect("inventory context"),
            )],
        })
        .await
        .expect("record inventory context");
    let acknowledgement = store
        .reconcile_mailbox_inventory(notification.clone())
        .await
        .expect("acknowledge inventory");
    assert_eq!(
        acknowledgement
            .bound_subscriptions
            .iter()
            .map(|subscription| {
                (
                    subscription.message_id.as_str(),
                    subscription.state,
                    subscription.bound_turn_id.as_deref(),
                )
            })
            .collect::<Vec<_>>(),
        vec![(
            subscription_id.as_str(),
            MailboxFinalSubscriptionState::Bound,
            Some(notification.id.as_str()),
        )],
    );

    control.revoke_response_observations_for_child(receiver);
    assert_eq!(
        control.response_observation_turn_final_response(
            sender,
            receiver_thread.session.presentation_id(),
            &notification.id,
        ),
        None,
    );
    let retried = control
        .accept_mailbox_agent_input(
            sender,
            sender_turn_id,
            call_id,
            receiver,
            text_input("bind to the exact inventory turn"),
            MailboxFinalSubscriptionRequest::Wake,
        )
        .await
        .expect("idempotent mailbox retry");
    assert_eq!(
        store
            .lookup_mailbox_input(receiver, &submission_key)
            .await
            .expect("read retried message"),
        Some(retried),
    );

    tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 5), async {
        loop {
            if control.response_observation_turn_final_response(
                sender,
                receiver_thread.session.presentation_id(),
                &notification.id,
            ) == Some(crate::agent::response_observation::FinalResponseObservation::Wake)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await
    .expect("detached retry binder installs exact bound receiver turn");
    root.close_agent(
        &receiver.to_string(),
        UserAgentResponseHandling::Presentation,
    )
    .await
    .expect("close receiver");
    root.shutdown_and_wait().await.expect("shutdown root");
}

#[tokio::test]
async fn explicit_live_wake_resume_retires_pending_mailbox_final_subscription() {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::Collab).expect("V1");
    config
        .features
        .disable(Feature::MultiAgentV2)
        .expect("not V2");
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (root_id, root) = harness.start_thread().await;
    let receiver = root
        .spawn_agent(UserAgentSpawnOptions::default())
        .await
        .expect("receiver")
        .target_thread_id;
    let call_id = "explicit-live-resume";
    let sender_turn_id = "explicit-live-resume-origin";
    assert!(root.session.begin_agent_response_turn(sender_turn_id));
    let accepted = root
        .session
        .services
        .agent_control
        .accept_mailbox_agent_input(
            root.session.presentation_id(),
            sender_turn_id,
            call_id,
            receiver,
            text_input("explicit wake takes precedence"),
            MailboxFinalSubscriptionRequest::Wake,
        )
        .await
        .expect("conditional mailbox acceptance");
    let subscription_id = accepted
        .final_subscription
        .as_ref()
        .expect("conditional subscription")
        .message_id
        .clone();
    assert_eq!(
        accepted
            .final_subscription
            .as_ref()
            .map(|subscription| subscription.state),
        Some(MailboxFinalSubscriptionState::Pending),
    );

    root.resume_agent(
        &receiver.to_string(),
        /*task*/ None,
        UserAgentResponseHandling::Wake,
    )
    .await
    .expect("explicit live wake resume");

    let key = serde_json::to_string(&("v1-send-input-mailbox", root_id, sender_turn_id, call_id))
        .expect("submission key");
    let retired = root
        .session
        .services
        .thread_store
        .lookup_mailbox_input(receiver, &key)
        .await
        .expect("read mailbox acceptance")
        .expect("accepted mailbox row");
    assert_eq!(
        retired
            .final_subscription
            .as_ref()
            .map(|subscription| subscription.state),
        Some(MailboxFinalSubscriptionState::Superseded),
    );
    let receiver_thread = harness
        .manager
        .get_thread(receiver)
        .await
        .expect("live receiver");
    let observation = root
        .session
        .services
        .agent_control
        .response_observation_audit_snapshots(
            root.session.presentation_id(),
            receiver_thread.session.presentation_id(),
            None,
        )
        .into_iter()
        .next()
        .expect("durable explicit wake observation");
    assert_eq!(
        (
            observation.final_delivery,
            observation.mailbox_final_subscription_message_id,
            observation.mailbox_final_subscription_suppressed_message_id,
        ),
        (
            codex_protocol::protocol::AgentResponseFinalDelivery::Wake,
            None,
            Some(subscription_id),
        ),
    );
    root.close_agent(
        &receiver.to_string(),
        UserAgentResponseHandling::Presentation,
    )
    .await
    .expect("close receiver");
    root.shutdown_and_wait().await.expect("shutdown root");
}

#[tokio::test]
async fn unloaded_receiver_recovery_runs_after_thread_registration() {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::Collab).expect("V1");
    config
        .features
        .disable(Feature::MultiAgentV2)
        .expect("not V2");
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (_, root) = harness.start_thread().await;
    let receiver = root
        .spawn_agent(UserAgentSpawnOptions::default())
        .await
        .expect("receiver")
        .target_thread_id;
    let receiver_thread = harness
        .manager
        .get_thread(receiver)
        .await
        .expect("loaded receiver");
    persist_thread_for_tree_resume(&receiver_thread, "persist before non-lifecycle unload").await;
    receiver_thread
        .shutdown_and_wait()
        .await
        .expect("unload receiver runtime");
    harness.manager.remove_thread(&receiver).await;
    assert!(harness.manager.get_thread(receiver).await.is_err());

    let control = root.session.services.agent_control.clone();
    let sender = root.session.presentation_id();
    let sender_turn_id = "unloaded-receiver-recovery";
    let call_id = "unloaded-receiver-recovery-call";
    control.revoke_response_observations_for_child(receiver);
    assert!(root.session.begin_agent_response_turn(sender_turn_id));
    let accepted = control
        .accept_mailbox_agent_input(
            sender,
            sender_turn_id,
            call_id,
            receiver,
            text_input("recover an unloaded receiver subscription"),
            MailboxFinalSubscriptionRequest::Wake,
        )
        .await
        .expect("accept while receiver runtime is absent");
    let subscription_id = accepted
        .final_subscription
        .as_ref()
        .expect("conditional subscription")
        .message_id
        .clone();
    assert_eq!(
        control.mailbox_final_subscription_message_id(
            sender,
            SessionPresentationId::new(receiver, uuid::Uuid::nil()),
        ),
        Some(subscription_id.clone()),
        "unloaded acceptance must not require a loaded receiver presentation",
    );

    let stored = root
        .session
        .services
        .thread_store
        .read_thread(ReadThreadParams {
            thread_id: receiver,
            include_archived: true,
            include_history: true,
        })
        .await
        .expect("read receiver history");
    let history = stored.history.expect("persisted receiver history");
    let resumed = harness
        .manager
        .resume_thread_with_history(
            harness.config.clone(),
            InitialHistory::Resumed(ResumedHistory {
                conversation_id: receiver,
                history: Arc::new(history.items),
                rollout_path: stored.rollout_path,
            }),
            harness.manager.auth_manager(),
            /*parent_trace*/ None,
            ClientMcpExtensions::default(),
        )
        .await
        .expect("reload receiver without explicit response policy");
    let receiver_presentation = resumed.thread.session.presentation_id();
    timeout(Duration::from_secs(/*secs*/ 5), async {
        loop {
            if control
                .mailbox_final_subscription_message_id(sender, receiver_presentation)
                .as_deref()
                == Some(subscription_id.as_str())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await
    .expect("registration-triggered recovery installs the durable pending subscription");

    resumed
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown resumed receiver");
    harness.manager.remove_thread(&receiver).await;
    root.shutdown_and_wait().await.expect("shutdown root");
}

#[tokio::test]
async fn unloaded_recipient_adoption_waits_for_authorized_mailbox_persistence() {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::Collab).expect("V1");
    config
        .features
        .disable(Feature::MultiAgentV2)
        .expect("not V2");
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (root_id, root) = harness.start_thread().await;
    let (_, adopter) = harness.start_thread().await;
    let spawned = root
        .spawn_agent(UserAgentSpawnOptions::default())
        .await
        .expect("receiver");
    let receiver = spawned.target_thread_id;
    let target = harness.manager.get_thread(receiver).await.expect("target");
    persist_thread_for_tree_resume(&target, "durable unloaded mailbox receiver").await;
    root.close_agent(
        &receiver.to_string(),
        UserAgentResponseHandling::Presentation,
    )
    .await
    .expect("unload before adoption");
    assert!(harness.manager.get_thread(receiver).await.is_err());
    let control = root.session.services.agent_control.clone();
    let sender = root.session.presentation_id();
    assert!(root.session.begin_agent_response_turn("mailbox-origin"));
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    *control
        .wait_agent_presentations
        .mailbox_acceptance_gate
        .lock()
        .expect("mailbox gate") = Some((reached_tx, release_rx));
    let accepting_control = control.clone();
    let acceptance = tokio::spawn(async move {
        accepting_control
            .accept_mailbox_agent_input(
                sender,
                "mailbox-origin",
                "mailbox-call",
                receiver,
                text_input("immutable mailbox content"),
                MailboxFinalSubscriptionRequest::None,
            )
            .await
    });
    timeout(Duration::from_secs(/*secs*/ 15), reached_rx)
        .await
        .expect("acceptance reaches pre-persist boundary")
        .expect("acceptance gate");
    let key = serde_json::to_string(&(
        "v1-send-input-mailbox",
        root_id,
        "mailbox-origin",
        "mailbox-call",
    ))
    .expect("invocation key");
    let store = &root.session.services.thread_store;
    assert_eq!(
        store
            .lookup_mailbox_input(receiver, &key)
            .await
            .expect("lookup before commit"),
        None,
    );

    // Poll the real adoption operation while acceptance is paused after attribution.
    // The same UUID lifecycle boundary must remain reserved through the mailbox insert.
    let selector = receiver.to_string();
    let adoption = adopter.resume_agent(
        &selector,
        Some("imported-mailbox".to_string()),
        UserAgentResponseHandling::Presentation,
    );
    tokio::pin!(adoption);
    assert!(futures::poll!(adoption.as_mut()).is_pending());
    assert!(
        control
            .upgrade()
            .expect("manager")
            .agent_lifecycle_lock(receiver)
            .try_lock_owned()
            .is_err(),
        "pre-persist acceptance must still exclude adoption",
    );
    assert_eq!(
        control
            .current_agent_alias(receiver)
            .await
            .expect("old owner")
            .map(|alias| alias.session_id),
        control.bound_session_id(),
    );
    release_tx.send(()).expect("release acceptance");
    let (accepted, adopted) = timeout(Duration::from_secs(/*secs*/ 15), async {
        tokio::join!(acceptance, adoption)
    })
    .await
    .expect("acceptance and adoption settle");
    let accepted = accepted
        .expect("acceptance task")
        .expect("accepted before transfer");
    let adopted = adopted.expect("adoption proceeds after acceptance");
    assert_eq!(adopted.target_thread_id, receiver);
    assert!(adopted.ownership_transfer.is_some());
    assert_eq!(
        control
            .current_agent_alias(receiver)
            .await
            .expect("new owner")
            .map(|alias| alias.session_id),
        adopter.session.services.agent_control.bound_session_id(),
    );
    let MailboxPayload::Agent { attribution, input } = &accepted.payload else {
        panic!("agent attribution");
    };
    assert_eq!(
        (
            attribution.sender.thread_id,
            attribution.recipient.thread_id,
            input
        ),
        (root_id, receiver, &text_input("immutable mailbox content")),
    );
    assert_eq!(
        store
            .lookup_mailbox_input(receiver, &key)
            .await
            .expect("stored acceptance"),
        Some(accepted.clone()),
    );
    assert_eq!(
        control
            .accept_mailbox_agent_input(
                sender,
                "mailbox-origin",
                "mailbox-call",
                receiver,
                text_input("immutable mailbox content"),
                MailboxFinalSubscriptionRequest::None,
            )
            .await
            .expect("retry is not a new grant after transfer"),
        accepted,
    );
    let error = control
        .accept_mailbox_agent_input(
            sender,
            "mailbox-origin",
            "fresh-call",
            receiver,
            text_input("not accepted after transfer"),
            MailboxFinalSubscriptionRequest::None,
        )
        .await
        .expect_err("fresh cross-root send must be denied");
    assert!(error.to_string().contains("cross-root"));
    adopter
        .close_agent(&selector, UserAgentResponseHandling::Presentation)
        .await
        .expect("close adopted receiver");
    root.shutdown_and_wait().await.expect("shutdown sender");
    adopter.shutdown_and_wait().await.expect("shutdown adopter");
}
