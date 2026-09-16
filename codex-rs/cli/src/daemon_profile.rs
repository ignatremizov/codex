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
    match command {
        AppServerDaemonSubcommand::Start | AppServerDaemonSubcommand::Restart => {
            let launch = effective_launch(overrides).await?;
            let command = if matches!(command, AppServerDaemonSubcommand::Start) {
                AppServerLifecycleCommand::Start
            } else {
                AppServerLifecycleCommand::Restart
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
        AppServerDaemonSubcommand::PidUpdateLoop => {
            let parsed = overrides.parse_overrides().map_err(anyhow::Error::msg)?;
            let mode = captured_updater_backend(&parsed)?;
            let home = find_codex_home()?;
            let selection = AuthFileSelection::from_env(&home)?;
            let launch = DaemonLaunchOptions::new(home.to_path_buf(), selection.clone(), mode)?;
            // Authentication must not gate updater identity or its local HTTP
            // policy. A malformed local config retains the existing fallback.
            let config = local_config_builder(home.to_path_buf(), selection, parsed)
                .build()
                .await
                .map_err(anyhow::Error::from);
            let http = crate::updater_http_client_factory(config);
            codex_app_server_daemon::run_pid_update_loop(launch, http).await
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
