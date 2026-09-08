use super::*;
use crate::ThreadManager;
use crate::config::test_config;
use crate::init_state_db;
use crate::thread_manager::StartThreadOptions;
use codex_agent_graph_store::AllocateAgentAliasRequest;
use codex_agent_graph_store::ThreadSpawnEdgeStatus;
use codex_agent_graph_store::TransferAgentAliasRequest;
use codex_login::CodexAuth;
use codex_protocol::SessionId;
use core_test_support::PathExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;

#[tokio::test]
async fn uuid_status_authority_tracks_durable_ownership_without_loading_sender()
-> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let mut config = test_config().await;
    config.codex_home = home.path().abs();
    config.cwd = config.codex_home.abs();
    let state_db = init_state_db(&config).await.expect("state database");
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Some(state_db),
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config))
        .await?;
    let control = root.thread.session.services.agent_control.clone();
    let store = control
        .upgrade()?
        .agent_graph_store()
        .expect("durable alias store");
    let sender = ThreadId::new();
    assert_eq!(
        control.v1_wait_status_authority(sender).await?,
        V1WaitStatusAuthority::MailOnly,
    );
    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: control.session_id(),
            parent_thread_id: root.thread_id,
            child_thread_id: sender,
            nickname: Some("MailSender".to_string()),
            task_path: None,
        })
        .await?;
    assert_eq!(
        control.v1_wait_status_authority(sender).await?,
        V1WaitStatusAuthority::Controlled,
    );
    store
        .set_agent_lifecycle_state(control.session_id(), sender, ThreadSpawnEdgeStatus::Closed)
        .await?;
    assert_eq!(
        control.v1_wait_status_authority(sender).await?,
        V1WaitStatusAuthority::Controlled,
    );
    let destination = ThreadId::new();
    let destination_session = SessionId::from(destination);
    store
        .ensure_agent_alias_namespace(destination_session)
        .await?;
    store
        .transfer_agent_alias(TransferAgentAliasRequest {
            expected_previous_session_id: Some(control.session_id()),
            expected_descendant_thread_ids: Vec::new(),
            new_session_id: destination_session,
            new_parent_thread_id: destination,
            thread_id: sender,
            nickname: None,
            task_path: None,
            authored_selector: sender.to_string(),
        })
        .await?;
    let before = store
        .find_agent_alias_by_thread(control.session_id(), sender)
        .await?;
    assert_eq!(
        control.v1_wait_status_authority(sender).await?,
        V1WaitStatusAuthority::MailOnly,
    );
    assert_eq!(
        store
            .find_agent_alias_by_thread(control.session_id(), sender)
            .await?,
        before,
    );
    assert!(manager.get_thread(sender).await.is_err());
    control.shutdown_live_agent(root.thread_id).await?;
    Ok(())
}
