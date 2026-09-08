use super::*;
use crate::UserAgentResponseHandling;
use crate::UserAgentSpawnOptions;
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
