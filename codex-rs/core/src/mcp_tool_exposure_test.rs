use std::collections::HashMap;
use std::sync::Arc;

use codex_connectors::metadata::sanitize_name;
use codex_features::Feature;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::EffectiveMcpServer;
use codex_mcp::ToolInfo;
use pretty_assertions::assert_eq;
use rmcp::model::JsonObject;
use rmcp::model::Tool;

use super::*;
use crate::config::test_config;
use crate::connectors::AppInfo;

fn make_connector(id: &str, name: &str) -> AppInfo {
    AppInfo {
        id: id.to_string(),
        name: name.to_string(),
        description: None,
        logo_url: None,
        logo_url_dark: None,
        distribution_channel: None,
        branding: None,
        app_metadata: None,
        labels: None,
        install_url: None,
        is_accessible: true,
        is_enabled: true,
        plugin_display_names: Vec::new(),
    }
}

fn make_mcp_tool(
    server_name: &str,
    tool_name: &str,
    connector_id: Option<&str>,
    connector_name: Option<&str>,
) -> ToolInfo {
    let tool_namespace = if server_name == CODEX_APPS_MCP_SERVER_NAME {
        connector_name
            .map(sanitize_name)
            .map(|connector_name| format!("mcp__{server_name}__{connector_name}"))
            .unwrap_or_else(|| server_name.to_string())
    } else {
        format!("mcp__{server_name}__")
    };

    ToolInfo {
        server_name: server_name.to_string(),
        supports_parallel_tool_calls: false,
        server_origin: None,
        callable_name: tool_name.to_string(),
        callable_namespace: tool_namespace,
        namespace_description: None,
        tool: Tool {
            name: tool_name.to_string().into(),
            title: None,
            description: Some(format!("Test tool: {tool_name}").into()),
            input_schema: Arc::new(JsonObject::default()),
            output_schema: None,
            annotations: None,
            execution: None,
            icons: None,
            meta: None,
        },
        connector_id: connector_id.map(str::to_string),
        connector_name: connector_name.map(str::to_string),
        plugin_display_names: Vec::new(),
    }
}

fn numbered_mcp_tools(count: usize) -> HashMap<String, ToolInfo> {
    (0..count)
        .map(|index| {
            let tool_name = format!("tool_{index}");
            (
                format!("mcp__rmcp__{tool_name}"),
                make_mcp_tool(
                    "rmcp", &tool_name, /*connector_id*/ None, /*connector_name*/ None,
                ),
            )
        })
        .collect()
}

fn effective_servers_for_config(config: &Config) -> HashMap<String, EffectiveMcpServer> {
    config
        .mcp_servers
        .get()
        .iter()
        .map(|(name, server)| (name.clone(), EffectiveMcpServer::configured(server.clone())))
        .collect()
}

fn effective_servers_for_tools(
    config: &Config,
    mcp_tools: &HashMap<String, ToolInfo>,
) -> HashMap<String, EffectiveMcpServer> {
    let mut effective_servers = effective_servers_for_config(config);
    for tool in mcp_tools.values() {
        effective_servers
            .entry(tool.server_name.clone())
            .or_insert_with(|| {
                EffectiveMcpServer::configured(stdio_mcp_server_config(
                    /*allow_implicit_invocation*/ true,
                ))
            });
    }
    effective_servers
}

fn stdio_mcp_server_config(
    allow_implicit_invocation: bool,
) -> codex_config::types::McpServerConfig {
    codex_config::types::McpServerConfig {
        transport: codex_config::types::McpServerTransportConfig::Stdio {
            command: "echo".to_string(),
            args: Vec::new(),
            env: None,
            env_vars: Vec::new(),
            cwd: None,
        },
        environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
        enabled: true,
        required: false,
        supports_parallel_tool_calls: false,
        allow_implicit_invocation,
        disabled_reason: None,
        startup_timeout_sec: None,
        tool_timeout_sec: None,
        default_tools_approval_mode: None,
        enabled_tools: None,
        disabled_tools: None,
        scopes: None,
        oauth: None,
        oauth_resource: None,
        tools: HashMap::new(),
    }
}

#[tokio::test]
async fn directly_exposes_small_effective_tool_sets() {
    let config = test_config().await;
    let search_tool_enabled = true;
    let mcp_tools = numbered_mcp_tools(DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD - 1);

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        &mcp_tools,
        /*connectors*/ None,
        &[],
        &config,
        &effective_servers_for_tools(&config, &mcp_tools),
        search_tool_enabled,
    );

    let mut direct_tool_names: Vec<_> = exposure.direct_tools.keys().cloned().collect();
    direct_tool_names.sort();
    let mut expected_tool_names: Vec<_> = mcp_tools.keys().cloned().collect();
    expected_tool_names.sort();
    assert_eq!(direct_tool_names, expected_tool_names);
    assert!(exposure.deferred_tools.is_none());
}

#[tokio::test]
async fn searches_large_effective_tool_sets() {
    let config = test_config().await;
    let search_tool_enabled = true;
    let mcp_tools = numbered_mcp_tools(DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD);

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        &mcp_tools,
        /*connectors*/ None,
        &[],
        &config,
        &effective_servers_for_tools(&config, &mcp_tools),
        search_tool_enabled,
    );

    assert!(exposure.direct_tools.is_empty());
    let deferred_tools = exposure
        .deferred_tools
        .as_ref()
        .expect("large tool sets should be discoverable through tool_search");
    let mut deferred_tool_names: Vec<_> = deferred_tools.keys().cloned().collect();
    deferred_tool_names.sort();
    let mut expected_tool_names: Vec<_> = mcp_tools.keys().cloned().collect();
    expected_tool_names.sort();
    assert_eq!(deferred_tool_names, expected_tool_names);
}

#[tokio::test]
async fn directly_exposes_explicit_apps_without_deferred_overlap() {
    let config = test_config().await;
    let search_tool_enabled = true;
    let mut mcp_tools = numbered_mcp_tools(DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD - 1);
    mcp_tools.extend([(
        "mcp__codex_apps__calendar_create_event".to_string(),
        make_mcp_tool(
            CODEX_APPS_MCP_SERVER_NAME,
            "calendar_create_event",
            Some("calendar"),
            Some("Calendar"),
        ),
    )]);
    let connectors = vec![make_connector("calendar", "Calendar")];

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        &mcp_tools,
        Some(connectors.as_slice()),
        connectors.as_slice(),
        &config,
        &effective_servers_for_tools(&config, &mcp_tools),
        search_tool_enabled,
    );

    let mut tool_names: Vec<String> = exposure.direct_tools.into_keys().collect();
    tool_names.sort();
    assert_eq!(
        tool_names,
        vec!["mcp__codex_apps__calendar_create_event".to_string()]
    );
    assert_eq!(
        exposure.deferred_tools.as_ref().map(HashMap::len),
        Some(DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD - 1)
    );
    let deferred_tools = exposure
        .deferred_tools
        .as_ref()
        .expect("large tool sets should be discoverable through tool_search");
    assert!(
        tool_names
            .iter()
            .all(|direct_tool_name| !deferred_tools.contains_key(direct_tool_name)),
        "direct tools should not also be deferred: {tool_names:?}"
    );
    assert!(!deferred_tools.contains_key("mcp__codex_apps__calendar_create_event"));
    assert!(deferred_tools.contains_key("mcp__rmcp__tool_0"));
}

#[tokio::test]
async fn non_deferred_exposure_still_filters_codex_apps_to_explicit_connectors() {
    let config = test_config().await;
    let search_tool_enabled = false;
    let mcp_tools = HashMap::from([
        (
            "mcp__rmcp__tool".to_string(),
            make_mcp_tool(
                "rmcp", "tool", /*connector_id*/ None, /*connector_name*/ None,
            ),
        ),
        (
            "mcp__codex_apps__calendar_create_event".to_string(),
            make_mcp_tool(
                CODEX_APPS_MCP_SERVER_NAME,
                "calendar_create_event",
                Some("calendar"),
                Some("Calendar"),
            ),
        ),
        (
            "mcp__codex_apps__drive_list".to_string(),
            make_mcp_tool(
                CODEX_APPS_MCP_SERVER_NAME,
                "drive_list",
                Some("drive"),
                Some("Drive"),
            ),
        ),
    ]);

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        &mcp_tools,
        /*connectors*/ None,
        &[make_connector("calendar", "Calendar")],
        &config,
        &effective_servers_for_tools(&config, &mcp_tools),
        search_tool_enabled,
    );

    let mut tool_names: Vec<_> = exposure.direct_tools.keys().cloned().collect();
    tool_names.sort();
    assert_eq!(
        tool_names,
        vec![
            "mcp__codex_apps__calendar_create_event".to_string(),
            "mcp__rmcp__tool".to_string(),
        ]
    );
    assert!(exposure.deferred_tools.is_none());
}

#[tokio::test]
async fn tool_search_disabled_exposes_all_enabled_codex_apps_directly() {
    let config = test_config().await;
    let search_tool_enabled = false;
    let mcp_tools = HashMap::from([
        (
            "mcp__codex_apps__calendar_create_event".to_string(),
            make_mcp_tool(
                CODEX_APPS_MCP_SERVER_NAME,
                "calendar_create_event",
                Some("calendar"),
                Some("Calendar"),
            ),
        ),
        (
            "mcp__codex_apps__drive_list".to_string(),
            make_mcp_tool(
                CODEX_APPS_MCP_SERVER_NAME,
                "drive_list",
                Some("drive"),
                Some("Drive"),
            ),
        ),
    ]);
    let connectors = vec![
        make_connector("calendar", "Calendar"),
        make_connector("drive", "Drive"),
    ];

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        &mcp_tools,
        Some(connectors.as_slice()),
        &[],
        &config,
        &effective_servers_for_tools(&config, &mcp_tools),
        search_tool_enabled,
    );

    let mut tool_names: Vec<_> = exposure.direct_tools.keys().cloned().collect();
    tool_names.sort();
    assert_eq!(
        tool_names,
        vec![
            "mcp__codex_apps__calendar_create_event".to_string(),
            "mcp__codex_apps__drive_list".to_string(),
        ]
    );
    assert!(exposure.deferred_tools.is_none());
}

#[tokio::test]
async fn small_codex_apps_inventory_exposes_enabled_apps_directly() {
    let config = test_config().await;
    let search_tool_enabled = true;
    let mcp_tools = HashMap::from([(
        "mcp__codex_apps__calendar_create_event".to_string(),
        make_mcp_tool(
            CODEX_APPS_MCP_SERVER_NAME,
            "calendar_create_event",
            Some("calendar"),
            Some("Calendar"),
        ),
    )]);
    let connectors = vec![make_connector("calendar", "Calendar")];

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        &mcp_tools,
        Some(connectors.as_slice()),
        &[],
        &config,
        &effective_servers_for_tools(&config, &mcp_tools),
        search_tool_enabled,
    );

    let mut tool_names: Vec<_> = exposure.direct_tools.keys().cloned().collect();
    tool_names.sort();
    assert_eq!(
        tool_names,
        vec!["mcp__codex_apps__calendar_create_event".to_string()]
    );
    assert!(exposure.deferred_tools.is_none());
}

#[tokio::test]
async fn always_defer_feature_preserves_explicit_apps() {
    let mut config = test_config().await;
    config
        .features
        .enable(Feature::ToolSearchAlwaysDeferMcpTools)
        .expect("test config should allow feature update");
    let search_tool_enabled = true;
    let mcp_tools = HashMap::from([
        (
            "mcp__rmcp__tool".to_string(),
            make_mcp_tool(
                "rmcp", "tool", /*connector_id*/ None, /*connector_name*/ None,
            ),
        ),
        (
            "mcp__codex_apps__calendar_create_event".to_string(),
            make_mcp_tool(
                CODEX_APPS_MCP_SERVER_NAME,
                "calendar_create_event",
                Some("calendar"),
                Some("Calendar"),
            ),
        ),
    ]);
    let connectors = vec![make_connector("calendar", "Calendar")];

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        &mcp_tools,
        Some(connectors.as_slice()),
        connectors.as_slice(),
        &config,
        &effective_servers_for_tools(&config, &mcp_tools),
        search_tool_enabled,
    );

    let mut direct_tool_names: Vec<String> = exposure.direct_tools.into_keys().collect();
    direct_tool_names.sort();
    assert_eq!(
        direct_tool_names,
        vec!["mcp__codex_apps__calendar_create_event".to_string()]
    );
    let deferred_tools = exposure
        .deferred_tools
        .as_ref()
        .expect("MCP tools should be discoverable through tool_search");
    assert!(deferred_tools.contains_key("mcp__rmcp__tool"));
    assert!(!deferred_tools.contains_key("mcp__codex_apps__calendar_create_event"));
}

#[tokio::test]
async fn hidden_servers_are_hidden_from_default_direct_exposure_but_still_tool_search_discoverable()
{
    let mut config = test_config().await;
    config
        .mcp_servers
        .set(HashMap::from([(
            "rmcp".to_string(),
            stdio_mcp_server_config(/*allow_implicit_invocation*/ false),
        )]))
        .expect("test config should accept MCP server config");
    let search_tool_enabled = true;
    let mcp_tools = HashMap::from([(
        "mcp__rmcp__tool".to_string(),
        make_mcp_tool(
            "rmcp", "tool", /*connector_id*/ None, /*connector_name*/ None,
        ),
    )]);

    let hidden = build_mcp_tool_exposure(
        &mcp_tools,
        &mcp_tools,
        /*connectors*/ None,
        &[],
        &config,
        &effective_servers_for_tools(&config, &mcp_tools),
        search_tool_enabled,
    );
    assert!(hidden.direct_tools.is_empty());
    assert!(
        hidden
            .deferred_tools
            .as_ref()
            .is_some_and(|tools| tools.contains_key("mcp__rmcp__tool")),
        "hidden MCP servers should remain deferred/callable rather than appearing in default direct exposure"
    );

    let after_mcp_use = build_mcp_tool_exposure(
        &mcp_tools,
        &mcp_tools,
        /*connectors*/ None,
        &[],
        &config,
        &effective_servers_for_tools(&config, &mcp_tools),
        search_tool_enabled,
    );
    assert!(
        after_mcp_use.direct_tools.is_empty(),
        "/mcp use injects prompt history only; it must not rewrite the top-level tool contract"
    );
    assert!(
        after_mcp_use
            .deferred_tools
            .as_ref()
            .is_some_and(|tools| tools.contains_key("mcp__rmcp__tool")),
        "hidden MCP servers stay deferred/callable after /mcp use so cacheable tool exposure is stable"
    );
}

#[tokio::test]
async fn hidden_effective_plugin_servers_are_hidden_from_default_direct_exposure() {
    let config = test_config().await;
    let search_tool_enabled = true;
    let mcp_tools = HashMap::from([(
        "mcp__plugin_docs__search".to_string(),
        make_mcp_tool(
            "plugin_docs",
            "search",
            /*connector_id*/ None,
            /*connector_name*/ None,
        ),
    )]);
    let effective_mcp_servers = HashMap::from([(
        "plugin_docs".to_string(),
        EffectiveMcpServer::configured(stdio_mcp_server_config(
            /*allow_implicit_invocation*/ false,
        )),
    )]);

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        &mcp_tools,
        /*connectors*/ None,
        &[],
        &config,
        &effective_mcp_servers,
        search_tool_enabled,
    );

    assert!(exposure.direct_tools.is_empty());
    assert!(
        exposure
            .deferred_tools
            .as_ref()
            .is_some_and(|tools| tools.contains_key("mcp__plugin_docs__search")),
        "plugin-provided MCP servers with allow_implicit_invocation = false must be hidden from default direct exposure while remaining discoverable through tool_search"
    );
}

#[tokio::test]
async fn session_start_implicit_exposure_is_not_removed_by_later_hidden_config_reload() {
    let mut latest_disk_config = test_config().await;
    latest_disk_config
        .mcp_servers
        .set(HashMap::from([(
            "rmcp".to_string(),
            stdio_mcp_server_config(/*allow_implicit_invocation*/ false),
        )]))
        .expect("test config should accept MCP server config");
    let session_start_effective_mcp_servers = HashMap::from([(
        "rmcp".to_string(),
        EffectiveMcpServer::configured(stdio_mcp_server_config(
            /*allow_implicit_invocation*/ true,
        )),
    )]);
    let search_tool_enabled = true;
    let mcp_tools = HashMap::from([(
        "mcp__rmcp__tool".to_string(),
        make_mcp_tool(
            "rmcp", "tool", /*connector_id*/ None, /*connector_name*/ None,
        ),
    )]);

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        &mcp_tools,
        /*connectors*/ None,
        &[],
        &latest_disk_config,
        &session_start_effective_mcp_servers,
        search_tool_enabled,
    );

    assert!(
        exposure.direct_tools.contains_key("mcp__rmcp__tool"),
        "CACHE INVARIANT: if an MCP server was implicitly visible at the session exposure boundary, a later config reload that would make it hidden must not retroactively remove it from the existing session's default model-visible contract"
    );
}

#[tokio::test]
async fn servers_added_after_session_start_are_not_added_to_default_direct_exposure() {
    let latest_disk_config = test_config().await;
    let session_start_effective_mcp_servers = HashMap::new();
    let search_tool_enabled = true;
    let mcp_tools = HashMap::from([(
        "mcp__new_server__tool".to_string(),
        make_mcp_tool(
            "new_server",
            "tool",
            /*connector_id*/ None,
            /*connector_name*/ None,
        ),
    )]);

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        &mcp_tools,
        /*connectors*/ None,
        &[],
        &latest_disk_config,
        &session_start_effective_mcp_servers,
        search_tool_enabled,
    );

    assert!(
        exposure.direct_tools.is_empty(),
        "CACHE INVARIANT: MCP servers added by a later reload are live/deferred/callable runtime state, but they must not be added to the existing session's default direct tool contract"
    );
    assert!(
        exposure
            .deferred_tools
            .as_ref()
            .is_some_and(|tools| tools.contains_key("mcp__new_server__tool")),
        "new MCP runtime tools should remain discoverable/callable through tool_search without changing the session-start direct contract"
    );
}

#[tokio::test]
async fn hidden_tools_do_not_force_visible_tools_into_deferred_exposure() {
    let config = test_config().await;
    let search_tool_enabled = true;
    let mut mcp_tools = numbered_mcp_tools(DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD - 1);
    mcp_tools.insert(
        "mcp__hidden__tool".to_string(),
        make_mcp_tool(
            "hidden", "tool", /*connector_id*/ None, /*connector_name*/ None,
        ),
    );
    let mut session_start_effective_mcp_servers = effective_servers_for_tools(&config, &mcp_tools);
    session_start_effective_mcp_servers.insert(
        "hidden".to_string(),
        EffectiveMcpServer::configured(stdio_mcp_server_config(
            /*allow_implicit_invocation*/ false,
        )),
    );

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        &mcp_tools,
        /*connectors*/ None,
        &[],
        &config,
        &session_start_effective_mcp_servers,
        search_tool_enabled,
    );

    assert_eq!(
        exposure.direct_tools.len(),
        DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD - 1,
        "hidden-by-default MCP tools must not count toward the direct-exposure threshold for unrelated visible MCP tools"
    );
    assert!(!exposure.direct_tools.contains_key("mcp__hidden__tool"));
    assert!(
        exposure
            .deferred_tools
            .as_ref()
            .is_some_and(|tools| tools.contains_key("mcp__hidden__tool")),
        "hidden MCP tools remain discoverable through tool_search"
    );
}

#[tokio::test]
async fn session_start_direct_tools_remain_direct_after_live_reload_removes_server() {
    let config = test_config().await;
    let search_tool_enabled = true;
    let session_start_mcp_tools = HashMap::from([(
        "mcp__rmcp__tool".to_string(),
        make_mcp_tool(
            "rmcp", "tool", /*connector_id*/ None, /*connector_name*/ None,
        ),
    )]);
    let session_start_effective_mcp_servers = HashMap::from([(
        "rmcp".to_string(),
        EffectiveMcpServer::configured(stdio_mcp_server_config(
            /*allow_implicit_invocation*/ true,
        )),
    )]);
    let live_mcp_tools = HashMap::new();

    let exposure = build_mcp_tool_exposure(
        &live_mcp_tools,
        &session_start_mcp_tools,
        /*connectors*/ None,
        &[],
        &config,
        &session_start_effective_mcp_servers,
        search_tool_enabled,
    );

    assert!(
        exposure.direct_tools.contains_key("mcp__rmcp__tool"),
        "CACHE INVARIANT: direct MCP tool specs that were present at session start must remain in the existing session's default direct contract even if a later runtime reload removes that server"
    );
}
