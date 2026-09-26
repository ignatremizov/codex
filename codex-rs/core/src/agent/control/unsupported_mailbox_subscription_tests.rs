use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn v1_child_spawn_succeeds_without_mailbox_subscription_lookup_support() {
    let (home, config) = test_config().await;
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        /*state_db*/ None,
    );
    let mut harness = AgentControlHarness {
        _home: home,
        config,
        state_db: None,
        control: manager.agent_control(),
        manager,
    };
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    harness.control = parent_thread.session.services.agent_control.clone();

    let child_thread_id = harness
        .spawn_anonymous_child(parent_thread_id, SpawnAgentOptions::default())
        .await;
    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be published");

    assert_eq!(
        parent_thread
            .session
            .services
            .agent_control
            .completion_parent_for_child(child_thread.session.presentation_id(), parent_thread_id,),
        Some(parent_thread.session.presentation_id()),
    );
}
