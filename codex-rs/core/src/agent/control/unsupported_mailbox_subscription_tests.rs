use super::*;

#[tokio::test]
async fn v1_child_spawn_succeeds_without_mailbox_subscription_lookup_support() {
    let harness = AgentControlHarness::new_without_state_db().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .spawn_anonymous_child(parent_thread_id, SpawnAgentOptions::default())
        .await;
    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be published");

    assert!(harness.control.has_completion_watcher(
        parent_thread.session.presentation_id(),
        child_thread.session.presentation_id(),
    ));
}
