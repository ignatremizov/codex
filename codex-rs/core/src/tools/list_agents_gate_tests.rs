use super::*;

#[tokio::test]
async fn v1_directory_requires_explicit_enablement_even_for_labeled_leaves() {
    let name = ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, "list_agents").to_string();
    for enabled in [false, true, false] {
        let plan = probe(|turn| {
            set_feature(turn, Feature::Collab, /*enabled*/ true);
            set_feature(turn, Feature::MultiAgentV2, /*enabled*/ false);
            update_config(turn, |config| {
                config.list_agents_enabled = enabled;
                config.agent_max_depth = 1;
            });
            turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: ThreadId::new(),
                depth: 1,
                agent_path: Some(AgentPath::try_from("/root/worker").expect("valid path")),
                agent_nickname: Some("Worker".to_string()),
                agent_role: Some("coder".to_string()),
            });
        })
        .await;

        let expected = if enabled {
            vec!["list_agents".to_string(), "send_input".to_string()]
        } else {
            vec!["send_input".to_string()]
        };
        assert_eq!(
            plan.namespace_function_names(MULTI_AGENT_V1_NAMESPACE),
            &expected
        );
        if enabled {
            plan.assert_registered_contains(&[&name]);
        } else {
            plan.assert_registered_lacks(&[&name]);
        }
        assert!(!plan.can_manage_children);
    }
}

#[tokio::test]
async fn v1_directory_opt_in_respects_deferred_search_and_disabled_agents() {
    let name = ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, "list_agents").to_string();
    let deferred = probe(|turn| {
        update_turn_settings_for_test(turn, |settings| {
            Arc::make_mut(&mut settings.model_info).supports_search_tool = true;
        });
        set_feature(turn, Feature::Collab, /*enabled*/ true);
        set_feature(turn, Feature::MultiAgentV2, /*enabled*/ false);
        update_config(turn, |config| config.list_agents_enabled = true);
    })
    .await;
    deferred.assert_registered_contains(&[&name]);
    assert_eq!(deferred.exposure(&name), ToolExposure::Deferred);
    deferred.assert_visible_lacks(&["list_agents", MULTI_AGENT_V1_NAMESPACE]);

    let disabled = probe(|turn| {
        set_feature(turn, Feature::Collab, /*enabled*/ false);
        set_feature(turn, Feature::MultiAgentV2, /*enabled*/ false);
        update_config(turn, |config| config.list_agents_enabled = true);
    })
    .await;
    disabled.assert_registered_lacks(&[&name]);
}

#[tokio::test]
async fn v1_directory_setting_does_not_change_v2_availability() {
    for enabled in [false, true] {
        let plan = probe(|turn| {
            set_feature(turn, Feature::MultiAgentV2, /*enabled*/ true);
            update_config(turn, |config| config.list_agents_enabled = enabled);
        })
        .await;
        assert!(
            plan.namespace_function_names(MULTI_AGENT_V2_NAMESPACE)
                .contains(&"list_agents".to_string())
        );
        plan.assert_registered_lacks(&[&ToolName::namespaced(
            MULTI_AGENT_V1_NAMESPACE,
            "list_agents",
        )
        .to_string()]);
    }
}
