use super::*;
use crate::legacy_core::config::ConfigBuilder;
use crate::legacy_core::config::ConfigOverrides;
use crate::terminal_visualization_instructions::TERMINAL_VISUALIZATION_INSTRUCTIONS;
use codex_config::LoaderOverrides;
use codex_features::Feature;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

async fn profile_config(
    temp_dir: &TempDir,
    cli_overrides: Vec<(String, toml::Value)>,
) -> Result<Config> {
    let codex_home = temp_dir.path().join("home");
    let profile_dir = codex_home.join("profiles");
    let cwd = temp_dir.path().join("work");
    std::fs::create_dir_all(&profile_dir)?;
    std::fs::create_dir_all(&cwd)?;
    std::fs::write(
        codex_home.join("config.toml"),
        "developer_instructions = 'Default developer instructions.'",
    )?;
    let profile_path = profile_dir.join("research.config.toml");
    std::fs::write(
        &profile_path,
        r#"
model = "research-model"
model_instructions_file = "base.md"
developer_instructions = "Research developer instructions."
"#,
    )?;
    std::fs::write(
        profile_dir.join("base.md"),
        "Research base instructions.\nPreserve this second line.",
    )?;
    let mut loader_overrides = LoaderOverrides::without_managed_config_for_tests();
    loader_overrides.user_config_path = Some(AbsolutePathBuf::from_absolute_path(profile_path)?);
    loader_overrides.user_config_profile = Some("research".parse()?);
    Ok(ConfigBuilder::default()
        .codex_home(codex_home)
        .loader_overrides(loader_overrides)
        .harness_overrides(ConfigOverrides {
            cwd: Some(cwd),
            ..ConfigOverrides::default()
        })
        .cli_overrides(cli_overrides)
        .build()
        .await?)
}

#[tokio::test]
async fn start_forwards_resolved_profile_instructions_with_optional_terminal_guidance() -> Result<()>
{
    let temp_dir = tempfile::tempdir()?;
    let mut config = profile_config(&temp_dir, Vec::new()).await?;
    assert_eq!(
        config.base_instructions_provenance,
        Some(BaseInstructionsProvenance::Custom)
    );
    config
        .features
        .disable(Feature::TerminalVisualizationInstructions)?;

    for mode in [ThreadParamsMode::Remote, ThreadParamsMode::Embedded] {
        let params = thread_start_params_from_config(
            &config, mode, /*remote_cwd_override*/ None, /*session_start_source*/ None,
        );
        assert_eq!(
            (
                params.model,
                params.base_instructions,
                params.developer_instructions
            ),
            (
                Some("research-model".to_string()),
                Some("Research base instructions.\nPreserve this second line.".to_string()),
                Some("Research developer instructions.".to_string()),
            )
        );
    }

    config
        .features
        .enable(Feature::TerminalVisualizationInstructions)?;
    let expected = Some(format!(
        "Research developer instructions.\n\n{TERMINAL_VISUALIZATION_INSTRUCTIONS}"
    ));
    for _ in 0..2 {
        let params = thread_start_params_from_config(
            &config,
            ThreadParamsMode::Remote,
            /*remote_cwd_override*/ None,
            /*session_start_source*/ None,
        );
        assert_eq!(params.developer_instructions, expected);
    }

    let thread_id = ThreadId::new();
    assert_eq!(
        thread_resume_params_from_config(
            config,
            thread_id,
            ThreadParamsMode::Remote,
            /*remote_cwd_override*/ None,
            ResumeModelSettings::PreserveExistingThread,
        ),
        ThreadResumeParams {
            thread_id: thread_id.to_string(),
            ..ThreadResumeParams::default()
        }
    );
    Ok(())
}

#[tokio::test]
async fn start_forwards_cli_instruction_overrides_over_selected_profile() -> Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let instructions_path = temp_dir.path().join("cli.md");
    std::fs::write(&instructions_path, "CLI base instructions.")?;
    let mut config = profile_config(
        &temp_dir,
        vec![
            (
                "model_instructions_file".to_string(),
                toml::Value::String(instructions_path.to_string_lossy().into_owned()),
            ),
            (
                "developer_instructions".to_string(),
                toml::Value::String("CLI developer instructions.".to_string()),
            ),
        ],
    )
    .await?;
    config
        .features
        .disable(Feature::TerminalVisualizationInstructions)?;
    let params = thread_start_params_from_config(
        &config,
        ThreadParamsMode::Remote,
        /*remote_cwd_override*/ None,
        /*session_start_source*/ None,
    );
    assert_eq!(
        (params.base_instructions, params.developer_instructions),
        (
            Some("CLI base instructions.".to_string()),
            Some("CLI developer instructions.".to_string()),
        )
    );
    Ok(())
}

#[tokio::test]
async fn start_does_not_forward_catalog_base_as_custom_instructions() -> Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let mut config = profile_config(&temp_dir, Vec::new()).await?;
    config.base_instructions = Some("Catalog instructions.".to_string());
    config.base_instructions_provenance = Some(BaseInstructionsProvenance::Model {
        model: "research-model".to_string(),
    });
    config
        .features
        .disable(Feature::TerminalVisualizationInstructions)?;
    let params = thread_start_params_from_config(
        &config,
        ThreadParamsMode::Remote,
        /*remote_cwd_override*/ None,
        /*session_start_source*/ None,
    );
    assert_eq!(
        (params.base_instructions, params.developer_instructions),
        (None, Some("Research developer instructions.".to_string()))
    );
    Ok(())
}
