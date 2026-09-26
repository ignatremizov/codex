use super::*;
use crate::legacy_core::config::ConfigBuilder;
use crate::legacy_core::config::edit::ConfigEditsBuilder;
use codex_config::LoaderOverrides;
use codex_config::types::SessionPickerViewMode;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn agent_preview_preferences_load_and_reload_locally() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let mut previous: Option<LocalSettings> = None;
    for (toml, expected) in [
        ("", (50, 0)),
        (
            "[tui]\nagent_prompt_preview_lines = 0\nagent_response_preview_lines = 2\n",
            (0, 2),
        ),
        (
            "[tui]\nagent_prompt_preview_lines = 1\nagent_response_preview_lines = 0\n",
            (1, 0),
        ),
    ] {
        std::fs::write(home.path().join("config.toml"), toml)?;
        let config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .loader_overrides(LoaderOverrides {
                ignore_project_config: true,
                ..LoaderOverrides::without_managed_config_for_tests()
            })
            .build()
            .await?;
        let local = previous.as_ref().map_or_else(
            || LocalSettings::from(&config),
            |previous| previous.reloaded(&config),
        );
        assert_eq!(
            (
                local.tui.agent_prompt_preview_lines,
                local.tui.agent_response_preview_lines
            ),
            expected
        );
        previous = Some(local);
    }
    Ok(())
}

#[tokio::test]
async fn command_preview_preferences_load_and_reload_locally() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let mut previous: Option<LocalSettings> = None;
    for (toml, expected) in [
        ("", (30, 50)),
        ("[tui]\n", (30, 50)),
        (
            "[tui]\ncommand_output_preview_lines = 0\nuser_shell_output_preview_lines = 2\n",
            (0, 2),
        ),
        (
            "[tui]\ncommand_output_preview_lines = 42\nuser_shell_output_preview_lines = 77\n",
            (42, 77),
        ),
    ] {
        std::fs::write(home.path().join("config.toml"), toml)?;
        let config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .loader_overrides(LoaderOverrides {
                ignore_project_config: true,
                ..LoaderOverrides::without_managed_config_for_tests()
            })
            .build()
            .await?;
        let local = previous.as_ref().map_or_else(
            || LocalSettings::from(&config),
            |previous| previous.reloaded(&config),
        );
        assert_eq!(
            (
                local.tui.command_output_preview_lines,
                local.tui.user_shell_output_preview_lines
            ),
            expected,
        );
        previous = Some(local);
    }
    Ok(())
}

#[tokio::test]
async fn dictation_preference_changes_only_on_local_reload() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let mut config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await?;
    config
        .features
        .enable(codex_features::Feature::VoiceTranscription)
        .expect("test config should allow feature update");
    let local = LocalSettings::from(&config);
    config
        .features
        .disable(codex_features::Feature::VoiceTranscription)
        .expect("test config should allow feature update");
    assert_eq!(
        (
            crate::dictation::keymap_features(&local).voice_transcription_enabled,
            local.reloaded(&config).voice_transcription_enabled,
        ),
        (crate::dictation::is_supported(), false),
    );
    Ok(())
}

#[tokio::test]
async fn compaction_detail_preference_tracks_local_reload() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let mut local: Option<LocalSettings> = None;
    for (config_text, expected) in [
        ("", true),
        ("[tui]\nshow_compact_summary = false\n", false),
        ("[tui]\nshow_compact_summary = true\n", true),
    ] {
        std::fs::write(home.path().join("config.toml"), config_text)?;
        let config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .loader_overrides(LoaderOverrides {
                ignore_project_config: true,
                ..LoaderOverrides::without_managed_config_for_tests()
            })
            .build()
            .await?;
        let reloaded = local.as_ref().map_or_else(
            || LocalSettings::from(&config),
            |local| local.reloaded(&config),
        );
        assert_eq!(reloaded.tui.show_compact_summary, expected);
        local = Some(reloaded);
    }
    Ok(())
}

#[tokio::test]
async fn launch_screen_mode_survives_configuration_reload() -> anyhow::Result<()> {
    use crate::transcript_mode::TranscriptMode;
    use codex_config::types::AltScreenMode;

    let home = tempfile::tempdir()?;
    let mut config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await?;
    config.tui_fullscreen_transcript = true;
    config.tui_alternate_screen = AltScreenMode::Auto;

    for (alternate_screen, owned, expected_mode, expected_alt) in [
        (true, true, TranscriptMode::Owned, AltScreenMode::Auto),
        (true, false, TranscriptMode::Terminal, AltScreenMode::Auto),
        (false, true, TranscriptMode::Terminal, AltScreenMode::Never),
    ] {
        let mut tui = crate::tui::test_support::make_test_tui()?;
        tui.set_alt_screen_enabled(alternate_screen);
        tui.set_owned_screen(owned)?;
        let local = LocalSettings::for_tui(&config, &tui);
        assert_eq!(
            (local.transcript_mode, local.tui.alternate_screen),
            (expected_mode, expected_alt),
        );

        let mut reloaded_config = config.clone();
        reloaded_config.tui_fullscreen_transcript = false;
        reloaded_config.tui_alternate_screen = AltScreenMode::Never;
        reloaded_config.tui_theme = Some("nord".into());
        let mut expected = LocalSettings::from(&reloaded_config);
        expected.transcript_mode = expected_mode;
        expected.tui.alternate_screen = expected_alt;
        assert_eq!(local.reloaded(&reloaded_config), expected);
        assert_eq!(LocalSettings::for_tui(&reloaded_config, &tui), expected);
        tui.set_owned_screen(/*owned*/ false)?;
    }
    Ok(())
}

#[tokio::test]
async fn system_motion_suppresses_animations_without_changing_saved_preferences()
-> anyhow::Result<()> {
    use crate::motion::MotionMode;

    for configured in [true, false] {
        let home = tempfile::tempdir()?;
        let config_text = format!("[tui]\nanimations = {configured}\n");
        std::fs::write(home.path().join("config.toml"), &config_text)?;
        let config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .loader_overrides(LoaderOverrides {
                ignore_project_config: true,
                ..LoaderOverrides::without_managed_config_for_tests()
            })
            .build()
            .await?;
        let animated = LocalSettings::with_accessibility_preferences(
            &config,
            MotionMode::Animated,
            MotionMode::Animated,
        );
        let reduced = LocalSettings::with_accessibility_preferences(
            &config,
            MotionMode::Reduced,
            MotionMode::Animated,
        );
        let mut expected = animated.clone();
        expected.tui.animations = false;
        assert_eq!(reduced, expected);
        assert_eq!(animated.tui.animations, configured);
        assert_eq!(config.animations, configured);
        assert_eq!(
            std::fs::read_to_string(home.path().join("config.toml"))?,
            config_text
        );
    }
    Ok(())
}

#[tokio::test]
async fn local_load_preserves_defaults_and_resolved_overrides() -> anyhow::Result<()> {
    for config_text in [
        "",
        r#"
[tui]
animations = false
whimsy = false # Retired: must not override effects or prevent strict loading.
show_tooltips = false
show_server_version_notice = false
auto_recap = false
fullscreen_transcript = true
vim_mode_default = true
terminal_resize_reflow_max_rows = 0
session_picker_view = "comfortable"
[tui.effects]
shimmer = false
[tui.rendering]
mermaid = false
math = false
tables = false
[history]
persistence = "none"
max_bytes = 4096
[notice]
fast_default_opt_out = true
"#,
    ] {
        let home = tempfile::tempdir()?;
        std::fs::write(home.path().join("config.toml"), config_text)?;
        let config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .strict_config(true)
            .loader_overrides(LoaderOverrides {
                ignore_project_config: true,
                ..LoaderOverrides::without_managed_config_for_tests()
            })
            .cli_overrides(vec![
                ("tui.disable_paste_burst".into(), true.into()),
                // The deprecated flag must not override or migrate into the TUI preference.
                (
                    "features.transcript_v2".into(),
                    config_text.is_empty().into(),
                ),
            ])
            .build()
            .await?;
        assert_eq!(config.startup_warnings, Vec::<String>::new());
        let local = LocalSettings::from(&config);
        let mut expected: Tui = toml::from_str("")?;
        expected.disable_paste_burst = Some(true);
        expected.session_picker_view = Some(SessionPickerViewMode::Dense);
        if !config_text.is_empty() {
            expected.animations = false;
            expected.effects.shimmer = false;
            expected.rendering = codex_config::types::TuiRendering {
                mermaid: false,
                math: false,
                tables: false,
            };
            expected.show_tooltips = false;
            expected.show_server_version_notice = false;
            expected.auto_recap = false;
            expected.fullscreen_transcript = true;
            expected.vim_mode_default = true;
            expected.terminal_resize_reflow_max_rows = Some(0);
            expected.session_picker_view = Some(SessionPickerViewMode::Comfortable);
        }
        assert_eq!(
            local.transcript_mode.is_owned(),
            expected.fullscreen_transcript
        );
        assert_eq!(local.tui, expected);
        assert_eq!(
            config
                .features
                .legacy_feature_usages()
                .map(|usage| usage.alias.as_str())
                .collect::<Vec<_>>(),
            vec!["features.transcript_v2"],
        );
        assert_eq!(
            local.terminal_resize_reflow(),
            config.terminal_resize_reflow
        );
        assert_eq!(
            (&local.history, &local.notices),
            (&config.history, &config.notices)
        );
    }
    Ok(())
}

#[tokio::test]
async fn local_writes_preserve_selected_user_file_and_home_destinations() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let selected = AbsolutePathBuf::from_absolute_path(home.path().join("work.config.toml"))?;
    std::fs::write(&selected, "[tui]\ntheme = \"dracula\"\n")?;
    let overrides = LoaderOverrides {
        user_config_path: Some(selected.clone()),
        user_config_profile: Some("work".parse()?),
        ignore_project_config: true,
        ..LoaderOverrides::without_managed_config_for_tests()
    };
    let config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .loader_overrides(overrides.clone())
        .build()
        .await?;
    let local = LocalSettings::from(&config);
    assert_eq!(local.user_config_path, selected);
    ConfigEditsBuilder::for_config_path(local.user_config_path.as_path())
        .with_edits([crate::legacy_core::config::edit::syntax_theme_edit("nord")])
        .apply()
        .await?;
    ConfigEditsBuilder::new(local.codex_home.as_path())
        .set_session_picker_view(SessionPickerViewMode::Comfortable)
        .apply()
        .await?;
    let reloaded = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .loader_overrides(overrides)
        .build()
        .await?;
    assert_eq!(
        LocalSettings::from(&reloaded).tui.theme.as_deref(),
        Some("nord")
    );
    let home_config: toml::Value =
        toml::from_str(&std::fs::read_to_string(home.path().join("config.toml"))?)?;
    assert_eq!(
        home_config["tui"]["session_picker_view"].as_str(),
        Some("comfortable")
    );
    assert_eq!(home_config["tui"].get("theme"), None);
    Ok(())
}

#[tokio::test]
async fn screen_reader_default_yields_to_preferences_on_reload() -> anyhow::Result<()> {
    use crate::motion::MotionMode;

    let home = tempfile::tempdir()?;
    for (config_text, expected) in [
        ("", false),
        ("[tui]\nanimations = true\n", true),
        ("[tui]\nanimations = false\n", false),
    ] {
        std::fs::write(home.path().join("config.toml"), config_text)?;
        let config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .loader_overrides(LoaderOverrides {
                ignore_project_config: true,
                ..LoaderOverrides::without_managed_config_for_tests()
            })
            .build()
            .await?;
        let local = LocalSettings::with_accessibility_preferences(
            &config,
            MotionMode::Animated,
            MotionMode::Reduced,
        );
        assert_eq!(local.tui.animations, expected);
    }
    Ok(())
}
