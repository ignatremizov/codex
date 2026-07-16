use super::ModelCatalogState;
use crate::context::ContextualUserFragment;
use codex_models_manager::bundled_models_response;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::protocol::MultiAgentVersion;
use pretty_assertions::assert_eq;

#[test]
fn displayed_catalog_preserves_models_across_runtime_tags() -> anyhow::Result<()> {
    let template = bundled_models_response()?
        .models
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("bundled catalog must contain a model"))?;
    let mut models = Vec::new();
    for (name, version) in [
        ("disabled", Some(MultiAgentVersion::Disabled)),
        ("legacy", Some(MultiAgentVersion::V1)),
        ("untagged", None),
        ("v2", Some(MultiAgentVersion::V2)),
    ] {
        let mut model = ModelPreset::from(template.clone());
        model.model = name.to_string();
        model.description = "Available override.".to_string();
        model.multi_agent_version = version;
        model.show_in_picker = true;
        model.supported_reasoning_efforts.clear();
        model.service_tiers.clear();
        models.push(model);
    }
    let mut hidden = models[0].clone();
    hidden.model = "hidden".to_string();
    hidden.show_in_picker = false;
    models.push(hidden);
    let actual = ModelCatalogState::new(&models).body();
    insta::assert_snapshot!(actual, @r"

    - `disabled`: Available override.
    - `legacy`: Available override.
    - `untagged`: Available override.
    - `v2`: Available override.
    ");
    for model in &mut models {
        model.multi_agent_version = Some(MultiAgentVersion::V2);
    }
    assert_eq!(ModelCatalogState::new(&models).body(), actual);
    Ok(())
}
