use super::ConfigManager;
use codex_login::AuthFileSelection;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn runtime_auth_selection_is_preserved_across_config_loads() -> std::io::Result<()> {
    let home = tempfile::tempdir()?;
    let selection =
        AuthFileSelection::resolve(home.path(), Some(std::ffi::OsStr::new("runtime-auth.json")))?;
    let manager = ConfigManager::without_managed_config_for_tests(home.path().to_path_buf())
        .auth_file_selection(selection.clone());
    let initial = manager
        .load_latest_config(Some(home.path().to_path_buf()))
        .await?;
    assert_eq!(initial.auth_file_selection, selection);

    let reloaded = manager
        .load_latest_config_with_session_layers(&initial.config_layer_stack, home.path())
        .await?;
    assert_eq!(reloaded.auth_file_selection, selection);
    assert_eq!(reloaded.auth_config(), initial.auth_config());

    let retained = manager
        .load_retained_session_config(&initial.config_layer_stack, home.path())
        .await?;
    assert_eq!(retained.auth_file_selection, selection);
    assert_eq!(retained.auth_config(), initial.auth_config());
    Ok(())
}
