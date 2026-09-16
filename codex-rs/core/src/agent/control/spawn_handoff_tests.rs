use super::*;
use crate::agent::control::spawn::SpawnInitialInput;
use futures::FutureExt;
use pretty_assertions::assert_eq;
use std::future::Future;
use std::task::Poll;

enum Handoff {
    ReceiverAlreadyDropped,
    QueuedResponseDropped,
}

#[test_case::test_case(Handoff::ReceiverAlreadyDropped; "failed_handoff")]
#[test_case::test_case(Handoff::QueuedResponseDropped; "queued_handoff")]
#[tokio::test]
async fn unacknowledged_prepared_spawn_keeps_rollback_and_parent_fence(handoff: Handoff) {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::Collab).expect("V1");
    config
        .features
        .disable(Feature::MultiAgentV2)
        .expect("not V2");
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (root_id, root) = harness.start_thread().await;
    let control = root.session.services.agent_control.clone();
    let prepared = control
        .spawn_agent_prepared(
            harness.config.clone(),
            SpawnInitialInput::None,
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                parent_thread_id: Some(root_id),
                ..Default::default()
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("spawn prepared through all asynchronous publication work");
    let child_id = prepared.agent.thread_id;
    let child = harness
        .manager
        .get_thread(child_id)
        .await
        .expect("published runtime");
    let mut unload = Box::pin(harness.manager.prepare_subtree_unload(child_id));
    assert!(unload.as_mut().now_or_never().is_none());
    let (sender, receiver) = tokio::sync::oneshot::channel();
    match handoff {
        Handoff::ReceiverAlreadyDropped => {
            drop(receiver);
            assert!(sender.send(prepared).is_err());
        }
        Handoff::QueuedResponseDropped => {
            assert!(sender.send(prepared).is_ok());
            // Sending did not commit ownership: only consumption by the original waiter does.
            assert!(unload.as_mut().now_or_never().is_none());
            drop(receiver);
        }
    }
    let mut subtree = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), unload)
        .await
        .expect("rollback releases membership fence")
        .expect("capture remaining root");
    assert_eq!(subtree.thread_ids(), [root_id].as_slice());
    assert!(child.io.durable_shutdown_succeeded());
    let state = control.upgrade().expect("manager");
    let alias = state
        .agent_graph_store()
        .expect("graph store")
        .find_current_agent_alias_by_thread(child_id)
        .await
        .expect("alias lookup")
        .expect("cancelled alias retained");
    assert_eq!(
        alias.state,
        codex_agent_graph_store::AgentAliasState::Closed
    );
    subtree.shutdown_and_remove().await.expect("unload root");
    assert!(harness.manager.list_thread_ids().await.is_empty());
}

#[tokio::test]
async fn sealed_child_prevents_recreating_its_absent_root_during_unload() {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::Collab).expect("V1");
    config
        .features
        .disable(Feature::MultiAgentV2)
        .expect("not V2");
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (root_id, root) = harness.start_thread().await;
    let child_id = root
        .spawn_agent(crate::UserAgentSpawnOptions::default())
        .await
        .expect("idle child")
        .target_thread_id;
    let child = harness.manager.get_thread(child_id).await.expect("child");
    root.ensure_rollout_materialized().await;
    root.flush_rollout().await.expect("persist root");
    let root_rollout = root.rollout_path().expect("root rollout");
    root.shutdown_durably_and_wait()
        .await
        .expect("stop root only");
    harness
        .manager
        .remove_thread_if_current(&root)
        .await
        .expect("unload root only");
    let delivery = child
        .session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("hold child drain");
    let mut subtree = harness
        .manager
        .prepare_subtree_unload(child_id)
        .await
        .expect("capture child");
    let mut unload = Box::pin(subtree.shutdown_and_remove());
    tokio::time::timeout(
        Duration::from_secs(/*secs*/ 5),
        std::future::poll_fn(|cx| {
            assert!(unload.as_mut().poll(cx).is_pending());
            if child.ensure_not_unloading().is_err() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }),
    )
    .await
    .expect("unload seals the child and releases lifecycle locks");
    let observer_control = child.session.services.agent_control.clone();
    let error = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 5),
        observer_control.resume_agent_from_rollout(
            harness.config.clone(),
            root_id,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
            ResponseObservationPolicy::default(),
        ),
    )
    .await
    .expect("controlled resume does not deadlock")
    .err()
    .expect("sealed observer cannot resume Main");
    assert!(matches!(
        error.details(),
        CodexErrorDetails::InvalidRequest(_)
    ));
    let error = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 5),
        harness.manager.resume_thread_from_rollout(
            harness.config.clone(),
            root_rollout,
            harness.manager.auth_manager(),
            /*parent_trace*/ None,
            ClientMcpExtensions::default(),
        ),
    )
    .await
    .expect("cold resume does not deadlock")
    .err()
    .expect("cold resume cannot recreate sealed subtree root");
    assert!(matches!(
        error.details(),
        CodexErrorDetails::InvalidRequest(_)
    ));
    assert_eq!(harness.manager.list_thread_ids().await, vec![child_id]);
    drop(delivery);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(/*secs*/ 5), unload)
            .await
            .expect("drain completes")
            .expect("unload"),
        vec![child_id],
    );
    assert!(harness.manager.list_thread_ids().await.is_empty());
}
