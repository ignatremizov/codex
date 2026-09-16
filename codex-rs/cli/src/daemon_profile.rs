use codex_app_server_daemon::DaemonLaunchOptions;
use codex_app_server_daemon::DaemonProfileLookup;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::config::find_codex_home;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthFileSelection;
use codex_utils_cli::CliConfigOverrides;

use crate::AppServerDaemonSubcommand;
use crate::AppServerLifecycleCommand;
use crate::AppServerRemoteControlMode;

pub(crate) fn launch_options(config: &Config) -> std::io::Result<DaemonLaunchOptions> {
    DaemonLaunchOptions::new(
        config.codex_home.to_path_buf(),
        config.auth_file_selection.clone(),
        config.cli_auth_credentials_store_mode,
    )
}

/// Local config has no auth-backed cloud loader. Running profile candidates
/// supply any backend requirement that was only available at server startup.
pub(crate) async fn existing_launch(
    overrides: &CliConfigOverrides,
) -> anyhow::Result<DaemonLaunchOptions> {
    let home = find_codex_home()?;
    let selection = AuthFileSelection::from_env(&home)?;
    let parsed = overrides.parse_overrides().map_err(anyhow::Error::msg)?;
    let lookup = backend_override(&parsed)?
        .map(DaemonProfileLookup::ExactBackend)
        .unwrap_or(DaemonProfileLookup::AnyBackend);
    let config = local_config_builder(home.to_path_buf(), selection, parsed)
        .build()
        .await?;
    codex_app_server_daemon::resolve_existing_launch(launch_options(&config)?, lookup).await
}

async fn effective_launch(overrides: &CliConfigOverrides) -> anyhow::Result<DaemonLaunchOptions> {
    let config = crate::cloud_config::load_config(overrides, Default::default()).await?;
    Ok(launch_options(&config)?)
}

pub(crate) async fn run_command(
    command: AppServerDaemonSubcommand,
    overrides: &CliConfigOverrides,
) -> anyhow::Result<()> {
    // Daemon lifecycle commands remain usable after the invoking project is deleted.
    if matches!(std::env::current_dir(), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    {
        std::env::set_current_dir(find_codex_home()?)?;
    }
    match command {
        AppServerDaemonSubcommand::Start | AppServerDaemonSubcommand::Restart => {
            let launch = effective_launch(overrides).await?;
            let command = if matches!(command, AppServerDaemonSubcommand::Start) {
                AppServerLifecycleCommand::Start
            } else {
                let parsed = overrides.parse_overrides().map_err(anyhow::Error::msg)?;
                let layer = codex_config::build_cli_overrides_layer(&parsed);
                let features = layer
                    .get("features")
                    .and_then(toml::Value::as_table)
                    .into_iter()
                    .flatten()
                    .map(|(name, value)| (name.as_str(), value))
                    .collect::<std::collections::BTreeMap<_, _>>();
                let mut feature_overrides = std::collections::BTreeMap::new();
                // Match Features::apply_map's alias precedence.
                for (&name, &value) in &features {
                    if name == "imagegenext" && features.contains_key("image_generation") {
                        continue;
                    }
                    let value = if let Some(table) = value.as_table() {
                        let Some(enabled) = table.get("enabled") else {
                            continue;
                        };
                        enabled
                    } else {
                        value
                    };
                    let feature = codex_features::feature_for_key(name).ok_or_else(|| {
                        anyhow::anyhow!("daemon restart requires a boolean feature toggle: {name}")
                    })?;
                    let enabled = value.as_bool().ok_or_else(|| {
                        anyhow::anyhow!(
                            "daemon restart requires a boolean enablement for features.{name}"
                        )
                    })?;
                    feature_overrides.insert(feature.key().to_string(), enabled);
                }
                AppServerLifecycleCommand::Restart { feature_overrides }
            };
            crate::print_app_server_daemon_output(&launch, command).await
        }
        AppServerDaemonSubcommand::Stop | AppServerDaemonSubcommand::Version => {
            let launch = existing_launch(overrides).await?;
            let command = if matches!(command, AppServerDaemonSubcommand::Stop) {
                AppServerLifecycleCommand::Stop
            } else {
                AppServerLifecycleCommand::Version
            };
            crate::print_app_server_daemon_output(&launch, command).await
        }
        AppServerDaemonSubcommand::Bootstrap(bootstrap) => {
            let launch = effective_launch(overrides).await?;
            let output = codex_app_server_daemon::bootstrap(
                &launch,
                codex_app_server_daemon::BootstrapOptions {
                    remote_control_enabled: bootstrap.remote_control,
                },
            )
            .await?;
            println!("{}", serde_json::to_string(&output)?);
            Ok(())
        }
        AppServerDaemonSubcommand::EnableRemoteControl => {
            let launch = effective_launch(overrides).await?;
            crate::print_app_server_remote_control_output(
                &launch,
                AppServerRemoteControlMode::Enabled,
            )
            .await
        }
        AppServerDaemonSubcommand::DisableRemoteControl => {
            let launch = existing_launch(overrides).await?;
            crate::print_app_server_remote_control_output(
                &launch,
                AppServerRemoteControlMode::Disabled,
            )
            .await
        }
        AppServerDaemonSubcommand::PidUpdateLoop {
            restore_release, ..
        } => {
            let parsed = overrides.parse_overrides().map_err(anyhow::Error::msg)?;
            let mode = captured_updater_backend(&parsed)?;
            let home = find_codex_home()?;
            let selection = AuthFileSelection::from_env(&home)?;
            let launch = DaemonLaunchOptions::new(home.to_path_buf(), selection.clone(), mode)?;
            // Resolve updater identity locally, and retain the current configuration
            // and network-policy validation before allowing updater traffic.
            let config = local_config_builder(home.to_path_buf(), selection, parsed)
                .build()
                .await
                .map_err(anyhow::Error::from);
            let http = crate::updater_http_client_factory(config)?;
            codex_app_server_daemon::run_pid_update_loop(launch, http, restore_release).await
        }
        AppServerDaemonSubcommand::Update { .. } => {
            unreachable!("update commands are handled by the CLI compatibility path")
        }
    }
}

fn local_config_builder(
    home: std::path::PathBuf,
    selection: AuthFileSelection,
    overrides: Vec<(String, toml::Value)>,
) -> ConfigBuilder {
    ConfigBuilder::default()
        .codex_home(home)
        .auth_file_selection(selection)
        .cli_overrides(overrides)
}

fn captured_updater_backend(
    overrides: &[(String, toml::Value)],
) -> anyhow::Result<AuthCredentialsStoreMode> {
    backend_override(overrides)?.ok_or_else(|| anyhow::anyhow!(
        "pid-update-loop requires the captured cli_auth_credentials_store override; start it with daemon bootstrap"
    ))
}

fn backend_override(
    overrides: &[(String, toml::Value)],
) -> anyhow::Result<Option<AuthCredentialsStoreMode>> {
    overrides
        .iter()
        .rev()
        .find(|(key, _)| key == "cli_auth_credentials_store")
        .map(|(_, value)| value.clone().try_into().map_err(anyhow::Error::from))
        .transpose()
}

#[cfg(test)]
#[path = "daemon_profile_tests.rs"]
mod tests;
