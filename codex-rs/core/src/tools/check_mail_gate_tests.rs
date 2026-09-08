use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn check_mail_remains_direct_with_search_and_never_enters_code_mode() {
    let name = ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, "check_mail");
    for mode in [ToolMode::Direct, ToolMode::CodeMode, ToolMode::CodeModeOnly] {
        let plan = probe(|turn| {
            set_features(turn, &[Feature::Collab, Feature::CodeMode]);
            set_feature(turn, Feature::MultiAgentV2, /*enabled*/ false);
            update_turn_settings_for_test(turn, |settings| {
                let model = Arc::make_mut(&mut settings.model_info);
                model.tool_mode = Some(mode);
                model.supports_search_tool = true;
            });
        })
        .await;
        plan.assert_registered_contains(&[&name.to_string()]);
        assert_eq!(
            plan.exposure(&name.to_string()),
            ToolExposure::DirectModelOnly
        );
        assert!(
            plan.namespace_function_names(MULTI_AGENT_V1_NAMESPACE)
                .contains(&"check_mail".to_string())
        );
        assert!(
            !plan
                .code_mode_tool_names
                .values()
                .any(|nested| nested == &name)
        );
    }
}

#[tokio::test]
async fn check_mail_is_absent_from_v2_and_disabled_collaboration() {
    let name = ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, "check_mail").to_string();
    let v2 = probe(|turn| {
        set_features(
            turn,
            &[Feature::Collab, Feature::MultiAgentV2, Feature::CodeMode],
        );
    })
    .await;
    v2.assert_registered_lacks(&[&name, "check_mail", "collaboration.check_mail"]);
    assert!(
        !v2.code_mode_tool_names
            .values()
            .any(|tool| tool.name == "check_mail")
    );
    let disabled = probe(|turn| {
        set_feature(turn, Feature::Collab, /*enabled*/ false);
        set_feature(turn, Feature::MultiAgentV2, /*enabled*/ false);
    })
    .await;
    disabled.assert_registered_lacks(&[&name, "check_mail"]);
}
