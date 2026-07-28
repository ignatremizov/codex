use super::*;
use crate::agent::control::CompletionPresentation;
use crate::session::sub_agent_completion::CompletionContextDelivery;
use crate::session::sub_agent_completion::CompletionContextPublication;
use crate::session::tests::make_session_and_context_with_rx;
use codex_history::RolloutItem;
use codex_protocol::AgentPath;
use codex_protocol::items::TurnItem;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::new_sub_agent_completion_context_response_item_id;
use codex_protocol::protocol::sub_agent_completion_item;
use codex_protocol::turn_input::TurnStartOptions;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::InMemoryThreadStoreCalls;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[tokio::test]
async fn runtime_publication_distinguishes_volatile_receipts_and_survives_dropped_waiters()
-> anyhow::Result<()> {
    let (mut session, _, events) = make_session_and_context_with_rx().await;
    let store = Arc::new(InMemoryThreadStore::default());
    Arc::get_mut(&mut session).unwrap().services.thread_store = store.clone();
    let event = Event {
        id: "runtime-event".into(),
        msg: EventMsg::ShutdownComplete,
    };
    let delivered = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&delivered);
    let permit = session.acquire_history_publication_barrier().await?;
    let receiver = session.dispatch_completion_publication(
        permit,
        vec![RolloutItem::EventMsg(event.msg.clone())],
        vec![event.clone()],
        |_| {},
        move || {
            observed.store(true, Ordering::Release);
        },
    )?;
    assert_eq!(
        session.publication_result(receiver).await?,
        CompletionPublicationReceipt::RuntimeOnly {
            primary_event: PrimaryEventEnqueue::Enqueued,
        }
    );
    assert_eq!(
        serde_json::to_value(events.recv().await?)?,
        serde_json::to_value(&event)?
    );
    assert!(delivered.load(Ordering::Acquire));

    let installed = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&installed);
    let permit = session.acquire_history_publication_barrier().await?;
    let receiver = session.dispatch_completion_publication(
        permit,
        Vec::new(),
        Vec::new(),
        move |_| {
            observed.store(true, Ordering::Release);
        },
        || {},
    )?;
    drop(receiver);
    let barrier = session.acquire_history_publication_barrier().await?;
    assert!(installed.load(Ordering::Acquire));
    drop(barrier);

    drop(events);
    delivered.store(false, Ordering::Release);
    let observed = Arc::clone(&delivered);
    let permit = session.acquire_history_publication_barrier().await?;
    let receiver = session.dispatch_completion_publication(
        permit,
        vec![RolloutItem::EventMsg(event.msg.clone())],
        vec![event],
        |_| {},
        move || {
            observed.store(true, Ordering::Release);
        },
    )?;
    assert_eq!(
        session.publication_result(receiver).await?,
        CompletionPublicationReceipt::RuntimeOnly {
            primary_event: PrimaryEventEnqueue::Closed,
        }
    );
    assert!(!delivered.load(Ordering::Acquire));
    assert_eq!(store.calls().await, InMemoryThreadStoreCalls::default());
    Ok(())
}

#[tokio::test]
async fn runtime_completion_consumption_requires_exact_positive_evidence() -> anyhow::Result<()> {
    let (mut session, turn, _) = make_session_and_context_with_rx().await;
    let store = Arc::new(InMemoryThreadStore::default());
    Arc::get_mut(&mut session).unwrap().services.thread_store = store.clone();
    let mut communication = InterAgentCommunication::new(
        AgentPath::try_from("/root/child").map_err(anyhow::Error::msg)?,
        AgentPath::root(),
        Vec::new(),
        "accepted runtime result".to_owned(),
        /*trigger_turn*/ false,
    );
    communication.id = Some(new_sub_agent_completion_context_response_item_id());
    let response = communication.to_model_input_item();
    let id = response.id().unwrap().clone();
    assert_eq!(session.load_completion_context_provenance(&id).await?, None);
    let accepted = session
        .submission_admission
        .try_accept_completion_delivery()
        .unwrap();
    assert_eq!(
        session
            .persist_completion_context(
                response.clone(),
                &accepted,
                CompletionContextDelivery::QueueOnly,
            )
            .await?,
        CompletionContextPublication::Published,
    );
    assert_eq!(
        session
            .persist_completion_context(
                response.clone(),
                &accepted,
                CompletionContextDelivery::QueueOnly,
            )
            .await?,
        CompletionContextPublication::AlreadyPublished,
    );
    let mut changed = response.clone();
    let ResponseItem::AgentMessage { content, .. } = &mut changed else {
        anyhow::bail!("completion fixture must use an agent message");
    };
    content.push(AgentMessageInputContent::InputText {
        text: "unacknowledged replacement".to_owned(),
    });
    assert!(
        session
            .persist_completion_context(changed, &accepted, CompletionContextDelivery::InstallNow)
            .await
            .is_err()
    );
    session
        .input_queue
        .enqueue_mailbox_communication(communication.clone(), TurnStartOptions::default())
        .await;
    drop(accepted);
    session.drain_completion_mailbox().await?;
    session
        .consume_completion_context(&communication, turn.model_info())
        .await?;
    assert_eq!(
        session.load_completion_context_provenance(&id).await?,
        Some(response.clone()),
    );
    let retained = {
        let state = session.state.lock().await;
        state
            .acknowledged_completion_contexts
            .iter()
            .map(|completion| (completion.item.item.clone(), completion.pending))
            .collect::<Vec<_>>()
    };
    assert_eq!(retained, vec![(response.clone(), false)]);
    assert_eq!(
        session
            .clone_history()
            .await
            .raw_items()
            .filter(|item| item.id() == Some(&id))
            .cloned()
            .collect::<Vec<_>>(),
        vec![response],
    );

    communication.id = Some(new_sub_agent_completion_context_response_item_id());
    session
        .input_queue
        .enqueue_mailbox_communication(communication.clone(), TurnStartOptions::default())
        .await;
    // The consumer refuses to steal an entry still queued for mailbox draining.
    // Claim it through the same drain boundary used for the valid receipt above.
    let error = session.drain_completion_mailbox().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("no matching publication provenance")
    );
    assert!(session.submission_admission.requires_reload());
    assert_eq!(store.calls().await, InMemoryThreadStoreCalls::default());
    Ok(())
}

#[tokio::test]
async fn runtime_presentation_reuses_the_exact_published_lifecycle() -> anyhow::Result<()> {
    let (mut session, _, events) = make_session_and_context_with_rx().await;
    let store = Arc::new(InMemoryThreadStore::default());
    Arc::get_mut(&mut session).unwrap().services.thread_store = store.clone();
    let presentation = CompletionPresentation {
        item: TurnItem::AgentMessage(
            sub_agent_completion_item(
                "/root/child",
                &AgentStatus::Completed(Some("runtime result".to_owned())),
            )
            .unwrap(),
        ),
        history_only_turn_id: uuid::Uuid::now_v7().to_string(),
    };
    let accepted = session
        .submission_admission
        .try_accept_completion_delivery()
        .unwrap();
    let mut deliveries = Vec::new();
    for _ in 0..2 {
        session
            .publish_completion_item(&presentation, &accepted)
            .await?;
        let events = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
            Ok::<_, anyhow::Error>(vec![events.recv().await?, events.recv().await?])
        })
        .await??;
        deliveries.push(serde_json::to_value(events)?);
    }
    assert_eq!(deliveries[0], deliveries[1]);
    assert_eq!(store.calls().await, InMemoryThreadStoreCalls::default());
    assert!(!session.submission_admission.requires_reload());
    Ok(())
}
