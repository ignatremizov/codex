use super::*;
use codex_protocol::protocol::InternalSessionSource;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn self_compaction_is_direct_for_root_and_native_child_in_every_tool_mode() {
    for mode in [ToolMode::Direct, ToolMode::CodeMode, ToolMode::CodeModeOnly] {
        for source in [
            SessionSource::Cli,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: ThreadId::new(),
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
        ] {
            let plan = probe(|turn| {
                turn.session_source = source;
                set_feature(turn, Feature::CodeMode, /*enabled*/ true);
                update_turn_settings_for_test(turn, |settings| {
                    Arc::make_mut(&mut settings.model_info).tool_mode = Some(mode);
                });
            })
            .await;
            plan.assert_visible_contains(&["compact"]);
            assert_eq!(plan.exposure("compact"), ToolExposure::DirectModelOnly);
            assert!(
                !plan
                    .code_mode_tool_names
                    .values()
                    .any(|name| name.name == "compact")
            );
        }
    }
}

#[tokio::test]
async fn private_helper_sources_cannot_request_self_compaction() {
    for source in [
        SessionSource::Internal(InternalSessionSource::Guardian),
        SessionSource::Internal(InternalSessionSource::MemoryConsolidation),
        SessionSource::SubAgent(SubAgentSource::Compact),
        SessionSource::SubAgent(SubAgentSource::Review),
    ] {
        let plan = probe(|turn| turn.session_source = source).await;
        plan.assert_registered_lacks(&["compact"]);
    }
}

#[tokio::test]
async fn disabling_the_model_compact_tool_prevents_registration() {
    let plan = probe(|turn| {
        update_config(turn, |config| config.compact_tool_enabled = false);
    })
    .await;
    plan.assert_registered_lacks(&["compact"]);
}
