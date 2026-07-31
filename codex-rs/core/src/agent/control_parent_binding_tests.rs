//! Native completion routes cannot cross root-control generations before their first binding.

use super::*;
use crate::thread_manager::ResumeThreadWithHistoryOptions;
use crate::thread_manager::ThreadRegistration;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn unbound_native_child_rejects_replacement_parent_control() {
    let mut harness = AgentControlHarness::new().await;
    let (parent_id, original) = harness.start_thread().await;
    harness.control = original
        .session
        .services
        .local_agent_runtime
        .control(original.session.session_id());
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
    // Public resume deliberately reuses the control retained by surviving children.
    // Exercise the foreign-control guard by explicitly loading this same root into
    // a fresh control, through the existing lower-level restoration boundary.
    let manager = harness.control.runtime.upgrade().expect("manager");
    let replacement = timeout(
        Duration::from_secs(/*secs*/ 5),
        manager.resume_thread_with_history_with_source(ResumeThreadWithHistoryOptions {
            registration: ThreadRegistration::Immediate,
            config: harness.config.clone(),
            initial_history: history,
            agent_control: harness.manager.agent_control(),
            session_source: original.session_source.clone(),
            parent_thread_id: None,
            environment_selections: None,
            inherited_environments: None,
            inherited_instructions: None,
            inherited_exec_policy: None,
            client_mcp_extensions_override: None,
        }),
    )
    .await
    .expect("replacement starts")
    .expect("resume replacement");
    assert_eq!(replacement.thread_id, parent_id);
    assert_ne!(
        replacement.thread.session.presentation_id(),
        original.session.presentation_id(),
    );
    let replacement_control = replacement
        .thread
        .session
        .services
        .local_agent_runtime
        .control(replacement.thread.session.session_id());
    assert!(!Arc::ptr_eq(
        &harness.control.runtime.registry,
        &replacement_control.runtime.registry
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
        .ensure_v1_completion_watcher(child_id, source)
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
