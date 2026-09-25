//! Native completion routes cannot cross root-control generations before their first binding.

use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn unbound_native_child_rejects_replacement_parent_control() {
    let mut harness = AgentControlHarness::new().await;
    let (parent_id, original) = harness.start_thread().await;
    harness.control = original.session.services.agent_control.clone();
    let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: parent_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });
    // This low-level spawn deliberately leaves the first watcher unregistered.
    let (child_id, child) = harness
        .start_thread_with_source(harness.config.clone(), source.clone())
        .await;
    let child_identity = child.session.presentation_id();
    assert_eq!(
        harness
            .control
            .completion_parent_for_child(child_identity, parent_id),
        None,
    );

    original.ensure_rollout_materialized().await;
    original.flush_rollout().await.expect("flush original");
    let history = crate::rollout::recorder::RolloutRecorder::get_rollout_history(
        &original.rollout_path().expect("original rollout"),
    )
    .await
    .expect("read original");
    assert!(harness.manager.remove_thread(&parent_id).await.is_some());
    timeout(
        Duration::from_secs(/*secs*/ 5),
        original.shutdown_and_wait(),
    )
    .await
    .expect("original shutdown completes")
    .expect("stop original");
    let replacement = timeout(
        Duration::from_secs(/*secs*/ 5),
        harness.manager.start_thread(StartThreadOptions {
            initial_history: history,
            ..StartThreadOptions::new(harness.config.clone())
        }),
    )
    .await
    .expect("replacement starts")
    .expect("resume replacement");
    assert_eq!(replacement.thread_id, parent_id);
    let replacement_control = &replacement.thread.session.services.agent_control;
    assert!(!Arc::ptr_eq(
        &harness.control.state,
        &replacement_control.state
    ));

    // A caller using the replacement's control must reject the old child even though
    // the captured parent and the source UUID both match that caller.
    let error = replacement_control
        .bind_completion_watcher_with_parent(
            &child,
            &replacement.thread,
            source.clone(),
            child_id.to_string(),
            /*child_agent_path*/ None,
            MultiAgentVersion::V1,
        )
        .expect_err("foreign child control must not bind");
    assert_matches!(
        error.details(),
        CodexErrorDetails::InvalidRequest(message)
            if message == "completion child and parent must belong to the binding control"
    );

    // The resume path uses the child's original control but resolves its parent by ID.
    // With no previous binding, the control check must reject the replacement parent.
    let error = replacement_control
        .ensure_native_v1_completion_watcher(child_id, source)
        .await
        .expect_err("replacement parent control must not adopt the native child");
    assert_matches!(
        error.details(),
        CodexErrorDetails::InvalidRequest(message)
            if message == "completion child and parent must belong to the binding control"
    );
    assert_eq!(
        (
            harness
                .control
                .completion_parent_for_child(child_identity, parent_id),
            replacement_control.completion_parent_for_child(child_identity, parent_id),
        ),
        (None, None),
    );

    timeout(Duration::from_secs(/*secs*/ 5), child.shutdown_and_wait())
        .await
        .expect("child shutdown completes")
        .expect("stop child");
    timeout(
        Duration::from_secs(/*secs*/ 5),
        replacement.thread.shutdown_and_wait(),
    )
    .await
    .expect("replacement shutdown completes")
    .expect("stop replacement");
}
