//! External history cannot borrow the provider or runtime policy of a same-UUID local thread.

use super::*;
use codex_extension_api::ThreadInstructionsProvider;
use codex_features::Feature;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::ThreadHistoryMode;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn external_fork_preserves_audit_identity_without_local_runtime_inheritance() {
    for supplied_provider in [false, true] {
        let home = tempdir().expect("active Codex home");
        let mut local_config = test_config().await;
        local_config.codex_home = home.path().abs();
        local_config.cwd = home.path().abs();
        local_config
            .features
            .enable(Feature::Collab)
            .expect("enable V1");
        local_config
            .features
            .enable(Feature::MultiAgentV2)
            .expect("local V2 runtime");
        let global = codex_extension_api::Instructions {
            text: "destination global policy".to_string(),
            source: None,
        };
        let local = codex_extension_api::Instructions {
            text: "unrelated local runtime policy".to_string(),
            source: None,
        };
        let destination = codex_extension_api::Instructions {
            text: "explicit destination thread policy".to_string(),
            source: None,
        };
        let mut manager = ThreadManager::with_models_provider_and_home_for_tests(
            CodexAuth::from_api_key("dummy"),
            local_config.model_provider.clone(),
            local_config.codex_home.to_path_buf(),
            Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        );
        Arc::get_mut(&mut manager.state)
            .expect("unique manager")
            .user_instructions_provider = Arc::new(ParentInstructionsProvider(global.clone()));
        let source = manager
            .start_thread(StartThreadOptions {
                environments: Some(Vec::new()),
                thread_instructions_provider: Some(Arc::new(ParentInstructionsProvider(
                    local.clone(),
                ))),
                ..StartThreadOptions::new(local_config.clone())
            })
            .await
            .expect("local same-ID runtime");
        assert_eq!(
            source.thread.multi_agent_version(),
            Some(MultiAgentVersion::V2)
        );
        let mut destination_config = local_config;
        destination_config
            .features
            .disable(Feature::MultiAgentV2)
            .expect("no destination V2 override");
        let foreign_base = BaseInstructions {
            text: "foreign saved base instructions".to_string(),
            provenance: None,
        };
        let history = InitialHistory::Resumed(ResumedHistory {
            conversation_id: source.thread_id,
            history: Arc::new(vec![RolloutItem::SessionMeta(SessionMetaLine {
                meta: SessionMeta {
                    id: source.thread_id,
                    session_id: source.thread_id.into(),
                    history_mode: ThreadHistoryMode::Legacy,
                    base_instructions: Some(foreign_base.clone()),
                    multi_agent_version: None,
                    ..Default::default()
                },
                git: None,
            })]),
            rollout_path: None,
        });
        let thread_provider = supplied_provider.then(|| {
            Arc::new(ParentInstructionsProvider(destination.clone()))
                as Arc<dyn ThreadInstructionsProvider>
        });
        let fork = manager
            .fork_thread_from_external_history(
                ForkSnapshot::Interrupted,
                StartThreadOptions {
                    environments: Some(Vec::new()),
                    thread_instructions_provider: thread_provider,
                    ..StartThreadOptions::new(destination_config)
                },
                history,
            )
            .await
            .expect("external history fork");
        let instructions = fork.thread.session.inherited_instructions().await;
        assert_eq!(
            (instructions.user, instructions.thread),
            (
                Some(global.clone()),
                supplied_provider.then_some(destination)
            )
        );
        assert_eq!(
            fork.thread.multi_agent_version(),
            Some(MultiAgentVersion::V1)
        );
        assert_eq!(
            fork.thread.session.get_base_instructions().await,
            foreign_base
        );
        let snapshot = fork.thread.config_snapshot().await;
        assert_eq!(snapshot.forked_from_thread_id, Some(source.thread_id));
        let source_instructions = source.thread.session.inherited_instructions().await;
        assert_eq!(
            (source_instructions.user, source_instructions.thread),
            (Some(global), Some(local))
        );
        assert!(Arc::ptr_eq(
            &manager
                .get_thread(source.thread_id)
                .await
                .expect("source retained"),
            &source.thread
        ));
        source
            .thread
            .shutdown_and_wait()
            .await
            .expect("close source");
        fork.thread.shutdown_and_wait().await.expect("close fork");
    }
}
