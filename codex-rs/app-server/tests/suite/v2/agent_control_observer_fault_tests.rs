//! A selected observer's unknown publication must remain unknown in the actor's audit.

use super::*;
use codex_app_server_protocol::AgentObservationMode;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::UserAgentControlAction;
use codex_app_server_protocol::UserAgentControlStatus;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(InMemoryThreadStoreFailure::SubAgentCompletionPrefix; "partial prefix")]
#[test_case(InMemoryThreadStoreFailure::SubAgentCompletionPresentationFlush; "lost flush receipt")]
#[tokio::test]
async fn selected_observer_failure_quarantines_observer_and_records_unknown_on_actor(
    failure: InMemoryThreadStoreFailure,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let FaultInjectionApp {
        client,
        root,
        store,
        _store_registration,
        _codex_home,
    } = start_fault_app(&server).await?;
    let spawned: AgentControlResponse = request(
        &client,
        ClientRequest::AgentControl {
            request_id: RequestId::Integer(2),
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: None,
                action: AgentControlAction::Spawn {
                    task: None,
                    role: None,
                    model: None,
                    reasoning_effort: None,
                    input: None,
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::Wake),
                },
            },
        },
    )
    .await?;
    assert_eq!(spawned.audit_warning, None);
    let AgentControlOutcome::Spawned {
        target_thread_id: observer,
        ..
    } = spawned.outcome
    else {
        anyhow::bail!("expected idle child observer");
    };
    let subscribed: AgentControlResponse = request(
        &client,
        ClientRequest::AgentControl {
            request_id: RequestId::Integer(3),
            params: AgentControlParams {
                source_thread_id: observer.clone(),
                authored_selector: None,
                action: AgentControlAction::Resume {
                    target: "Main".to_string(),
                    task: None,
                    response_handling: Some(AgentResponseHandling::Wake),
                },
            },
        },
    )
    .await?;
    assert_eq!(subscribed.audit_warning, None);
    assert!(matches!(
        subscribed.outcome,
        AgentControlOutcome::Resumed { .. }
    ));
    let before = store.calls().await.append_completion_items_and_flush;
    store
        .fail_observation_barrier_after(/*successful_barriers*/ 0, failure)
        .await;
    let error = timeout(
        READ_TIMEOUT,
        client.request(ClientRequest::AgentControl {
            request_id: RequestId::Integer(4),
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("Main".to_string()),
                action: AgentControlAction::Observe {
                    target: "Main".to_string(),
                    observer: Some(observer.clone()),
                    authored_observer_selector: Some(format!("id:{observer}")),
                    response_handling: AgentObservationMode::Passive,
                },
            },
        }),
    )
    .await??
    .expect_err("partial observation publication must not report success");
    assert!(error.message.contains("outcome unknown"), "{error:?}");
    assert!(store.calls().await.append_completion_items_and_flush > before);

    let read: ThreadReadResponse = request(
        &client,
        ClientRequest::ThreadRead {
            request_id: RequestId::Integer(5),
            params: ThreadReadParams {
                thread_id: root.thread.id.clone(),
                include_turns: true,
            },
        },
    )
    .await?;
    let audit = read
        .thread
        .turns
        .iter()
        .flat_map(|turn| &turn.items)
        .filter_map(|item| match item {
            ThreadItem::UserAgentControl {
                action: UserAgentControlAction::Observe,
                target_thread_id,
                observer_thread_id,
                authored_observer_selector,
                status,
                error,
                ..
            } => Some((
                target_thread_id.clone(),
                observer_thread_id.clone(),
                authored_observer_selector.clone(),
                *status,
                error
                    .as_deref()
                    .is_some_and(|error| error.contains("outcome unknown")),
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        audit,
        vec![(
            Some(root.thread.id),
            Some(observer.clone()),
            Some(format!("id:{observer}")),
            UserAgentControlStatus::Unknown,
            true,
        )]
    );
    let rejected = timeout(
        READ_TIMEOUT,
        client.request(ClientRequest::TurnStart {
            request_id: RequestId::Integer(6),
            params: TurnStartParams {
                thread_id: observer,
                input: vec![UserInput::Text {
                    text: "do not start work from an uncertain observer".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        }),
    )
    .await??
    .expect_err("selected observer requires canonical reload");
    assert!(
        rejected.message.contains("thread history must be reloaded"),
        "{rejected:?}"
    );
    assert!(
        server
            .received_requests()
            .await
            .context("model requests")?
            .iter()
            .all(|request| !request.url.path().ends_with("/responses"))
    );
    client.shutdown().await?;
    Ok(())
}
