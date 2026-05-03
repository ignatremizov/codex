use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use codex_connectors::metadata::sanitize_name;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::EffectiveMcpServer;
use codex_mcp::McpPluginAttribution;
use codex_mcp::McpServerRegistration;
use codex_mcp::ResolvedMcpCatalog;
use codex_mcp::ToolInfo;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use pretty_assertions::assert_eq;
use rmcp::model::JsonObject;
use rmcp::model::MetaObject;
use rmcp::model::Tool;

use super::*;
use crate::config::CONFIG_TOML_FILE;
use crate::config::Config;
use crate::config::ConfigBuilder;
use crate::config::test_config;
use crate::connectors::AppInfo;
use tempfile::tempdir;

fn make_connector(id: &str, name: &str) -> AppInfo {
    AppInfo {
        id: id.to_string(),
        name: name.to_string(),
        description: None,
        logo_url: None,
        logo_url_dark: None,
        icon_assets: None,
        icon_dark_assets: None,
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
    let callable_namespace = if server_name == CODEX_APPS_MCP_SERVER_NAME {
        connector_name
            .map(sanitize_name)
            .map(|connector_name| format!("mcp__{server_name}__{connector_name}"))
            .unwrap_or_else(|| server_name.to_string())
    } else {
        format!("mcp__{server_name}__")
    };
    make_mcp_tool_with_route(
        server_name,
        tool_name,
        &callable_namespace,
        tool_name,
        connector_id,
        connector_name,
    )
}

fn make_mcp_tool_with_route(
    server_name: &str,
    tool_name: &str,
    callable_namespace: &str,
    callable_name: &str,
    connector_id: Option<&str>,
    connector_name: Option<&str>,
) -> ToolInfo {
    let mut tool = Tool::new(
        tool_name.to_string(),
        format!("Test tool: {tool_name}"),
        Arc::new(JsonObject::default()),
    );
    tool.meta = Some(MetaObject(
        serde_json::json!({ "ui": { "visibility": ["model"] } })
            .as_object()
            .expect("metadata object")
            .clone(),
    ));

    ToolInfo {
        server_name: server_name.to_string(),
        supports_parallel_tool_calls: false,
        server_origin: None,
        callable_name: callable_name.to_string(),
        callable_namespace: callable_namespace.to_string(),
        namespace_description: None,
        tool,
        openai_file_input_optional_fields: Default::default(),
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

fn registered_exposures(
    tools: &[ToolInfo],
    config: &Config,
    apps_enabled: bool,
    mcp_server_catalog: &ResolvedMcpCatalog,
    search_tool_enabled: bool,
) -> HashMap<ToolName, ToolExposure> {
    let mut handlers = HashMap::new();
    let mut registry = ToolRegistry::default();
    append_mcp_tools(
        tools,
        McpToolRegistrationContext {
            session_start_mcp_servers: &effective_servers_for_config(config),
            config,
            apps_enabled,
            mcp_server_catalog,
            search_tool_enabled,
        },
        &mut handlers,
        &mut registry,
    );
    registry
        .entries()
        .map(|tool| (tool.runtime.tool_name(), tool.exposure))
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
        auth: Default::default(),
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
        omit_tools_from: None,
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
async fn agent_plugin_budget_hides_only_overflow_agent_tools() {
    let codex_home = tempdir().expect("tempdir should succeed");
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        "[mcp_servers.agent]\ncommand = \"echo\"\n",
    )
    .expect("write config");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("config should build");
    let agent_config = config.mcp_servers.get()["agent"].clone();
    let legacy_config = agent_config.clone();
    let mut catalog = ResolvedMcpCatalog::builder();
    catalog.register(McpServerRegistration::from_plugin(
        "agent".to_string(),
        McpPluginAttribution::agent_plugin("agent@test".to_string(), "Agent".to_string()),
        /*plugin_order*/ 0,
        agent_config,
    ));
    catalog.register(McpServerRegistration::from_plugin(
        "legacy".to_string(),
        McpPluginAttribution::new("legacy@test".to_string(), "Legacy".to_string()),
        /*plugin_order*/ 1,
        legacy_config,
    ));
    let catalog = catalog.build();
    let mut tools = (0..40)
        .map(|index| {
            let name = format!("tool_{index}");
            let mut tool = make_mcp_tool_with_route(
                "agent",
                &name,
                "mcp__agent",
                &name,
                /*connector_id*/ None,
                /*connector_name*/ None,
            );
            tool.namespace_description = Some("n".repeat(1_000));
            tool.tool.description = Some("d".repeat(1_000).into());
            tool
        })
        .collect::<Vec<_>>();
    let oversized_name = "x".repeat(MAX_AGENT_PLUGIN_MCP_SPEC_BYTES);
    let oversized_agent_tool = make_mcp_tool_with_route(
        "agent",
        "oversized_agent_tool",
        "mcp__agent",
        &oversized_name,
        /*connector_id*/ None,
        /*connector_name*/ None,
    );
    tools.push(oversized_agent_tool.clone());
    let legacy_tool = make_mcp_tool_with_route(
        "legacy",
        "legacy_tool",
        "mcp__legacy",
        &oversized_name,
        /*connector_id*/ None,
        /*connector_name*/ None,
    );
    tools.push(legacy_tool.clone());

    let exposures = registered_exposures(
        &tools, &config, /*apps_enabled*/ false, &catalog, /*search_tool_enabled*/ false,
    );
    let agent_exposures = tools[..40]
        .iter()
        .map(|tool| exposures[&tool.canonical_tool_name()])
        .collect::<Vec<_>>();

    assert!(agent_exposures.contains(&ToolExposure::Direct));
    assert!(agent_exposures.contains(&ToolExposure::Hidden));
    assert_eq!(
        exposures[&oversized_agent_tool.canonical_tool_name()],
        ToolExposure::Hidden
    );
    assert_eq!(
        exposures[&legacy_tool.canonical_tool_name()],
        ToolExposure::Direct
    );
}

#[tokio::test]
async fn cached_app_handlers_still_obey_current_apps_enablement_and_tool_policy() {
    let config = test_config().await;
    let codex_home = tempdir().expect("create restrictive config directory");
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        "[apps.calendar]\ndefault_tools_enabled = false\n",
    )
    .expect("write restrictive app policy");
    let restricted_config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("build restrictive app policy");
    let tools = [make_mcp_tool(
        CODEX_APPS_MCP_SERVER_NAME,
        "events/create",
        Some("calendar"),
        Some("Calendar"),
    )];
    let session_start_mcp_servers = HashMap::new();
    let mut handlers = HashMap::new();
    let catalog = ResolvedMcpCatalog::default();
    let mut allowed_registry = ToolRegistry::default();
    let allowed = append_mcp_tools(
        &tools,
        McpToolRegistrationContext {
            session_start_mcp_servers: &session_start_mcp_servers,
            config: &config,
            apps_enabled: true,
            mcp_server_catalog: &catalog,
            search_tool_enabled: false,
        },
        &mut handlers,
        &mut allowed_registry,
    );
    let cached_handler = &allowed_registry
        .entries()
        .next()
        .expect("allowed app tool should be registered")
        .runtime;

    let mut disabled_registry = ToolRegistry::default();
    let disabled = append_mcp_tools(
        &tools,
        McpToolRegistrationContext {
            session_start_mcp_servers: &session_start_mcp_servers,
            config: &config,
            apps_enabled: false,
            mcp_server_catalog: &catalog,
            search_tool_enabled: false,
        },
        &mut handlers,
        &mut disabled_registry,
    );
    let mut restricted_registry = ToolRegistry::default();
    let restricted = append_mcp_tools(
        &tools,
        McpToolRegistrationContext {
            session_start_mcp_servers: &session_start_mcp_servers,
            config: &restricted_config,
            apps_enabled: true,
            mcp_server_catalog: &catalog,
            search_tool_enabled: false,
        },
        &mut handlers,
        &mut restricted_registry,
    );
    let mut restored_registry = ToolRegistry::default();
    let restored = append_mcp_tools(
        &tools,
        McpToolRegistrationContext {
            session_start_mcp_servers: &session_start_mcp_servers,
            config: &config,
            apps_enabled: true,
            mcp_server_catalog: &catalog,
            search_tool_enabled: true,
        },
        &mut handlers,
        &mut restored_registry,
    );
    let restored_handler = restored_registry
        .entries()
        .next()
        .expect("restored app tool should be registered");

    assert_eq!(allowed, HashSet::from([tools[0].canonical_tool_name()]));
    assert!(disabled.is_empty());
    assert!(restricted.is_empty());
    assert_eq!(restored, allowed);
    assert!(Arc::ptr_eq(cached_handler, &restored_handler.runtime));
    assert_eq!(restored_handler.exposure, ToolExposure::Deferred);
}

#[tokio::test]
async fn directly_exposes_session_start_tool_sets_when_search_is_unavailable() {
    let config = test_config().await;
    let mcp_tools = numbered_mcp_tools(/*count*/ 2);

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        &mcp_tools,
        /*connectors*/ None,
        &[],
        &config,
        &effective_servers_for_tools(&config, &mcp_tools),
        /*search_tool_enabled*/ false,
    );

    assert_eq!(
        exposure.direct_tools.keys().collect::<HashSet<_>>(),
        mcp_tools.keys().collect::<HashSet<_>>()
    );
    assert!(exposure.deferred_tools.is_none());
}

#[tokio::test]
async fn defers_session_start_tool_sets_when_search_is_available() {
    let config = test_config().await;
    let mcp_tools = numbered_mcp_tools(/*count*/ 2);

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        &mcp_tools,
        /*connectors*/ None,
        &[],
        &config,
        &effective_servers_for_tools(&config, &mcp_tools),
        /*search_tool_enabled*/ true,
    );

    assert!(exposure.direct_tools.is_empty());
    assert_eq!(
        exposure
            .deferred_tools
            .as_ref()
            .expect("MCP tool sets should be deferred when search is available")
            .keys()
            .collect::<HashSet<_>>(),
        mcp_tools.keys().collect::<HashSet<_>>()
    );
}

#[tokio::test]
async fn hidden_by_default_servers_stay_deferred_and_not_direct() {
    let config = test_config().await;
    let mcp_tools = HashMap::from([(
        "mcp__rmcp__tool".to_string(),
        make_mcp_tool(
            "rmcp", "tool", /*connector_id*/ None, /*connector_name*/ None,
        ),
    )]);
    let effective_mcp_servers = HashMap::from([(
        "rmcp".to_string(),
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
        /*search_tool_enabled*/ false,
    );

    assert!(exposure.direct_tools.is_empty());
    assert!(
        exposure
            .deferred_tools
            .as_ref()
            .is_some_and(|tools| tools.contains_key("mcp__rmcp__tool"))
    );
}

#[tokio::test]
async fn servers_added_after_session_start_are_deferred_and_not_direct() {
    let config = test_config().await;
    let live_mcp_tools = HashMap::from([(
        "mcp__new_server__tool".to_string(),
        make_mcp_tool(
            "new_server",
            "tool",
            /*connector_id*/ None,
            /*connector_name*/ None,
        ),
    )]);

    let exposure = build_mcp_tool_exposure(
        &live_mcp_tools,
        &HashMap::new(),
        /*connectors*/ None,
        &[],
        &config,
        &HashMap::new(),
        /*search_tool_enabled*/ false,
    );

    assert!(exposure.direct_tools.is_empty());
    assert!(
        exposure
            .deferred_tools
            .as_ref()
            .is_some_and(|tools| tools.contains_key("mcp__new_server__tool"))
    );
}

#[tokio::test]
async fn session_start_direct_contract_uses_frozen_implicit_visibility() {
    let mut latest_config = test_config().await;
    latest_config
        .mcp_servers
        .set(HashMap::from([(
            "rmcp".to_string(),
            stdio_mcp_server_config(/*allow_implicit_invocation*/ false),
        )]))
        .expect("test config should accept MCP server config");
    let session_start_mcp_tools = HashMap::from([(
        "mcp__rmcp__tool".to_string(),
        make_mcp_tool(
            "rmcp", "tool", /*connector_id*/ None, /*connector_name*/ None,
        ),
    )]);
    let session_start_mcp_servers = HashMap::from([(
        "rmcp".to_string(),
        EffectiveMcpServer::configured(stdio_mcp_server_config(
            /*allow_implicit_invocation*/ true,
        )),
    )]);

    let exposure = build_mcp_tool_exposure(
        &session_start_mcp_tools,
        &session_start_mcp_tools,
        /*connectors*/ None,
        &[],
        &latest_config,
        &session_start_mcp_servers,
        /*search_tool_enabled*/ false,
    );

    assert!(exposure.direct_tools.contains_key("mcp__rmcp__tool"));
    assert!(exposure.deferred_tools.is_none());
}

#[tokio::test]
async fn session_start_direct_contract_survives_live_server_removal() {
    let config = test_config().await;
    let session_start_mcp_tools = HashMap::from([(
        "mcp__rmcp__tool".to_string(),
        make_mcp_tool(
            "rmcp", "tool", /*connector_id*/ None, /*connector_name*/ None,
        ),
    )]);
    let session_start_mcp_servers = HashMap::from([(
        "rmcp".to_string(),
        EffectiveMcpServer::configured(stdio_mcp_server_config(
            /*allow_implicit_invocation*/ true,
        )),
    )]);

    let exposure = build_mcp_tool_exposure(
        &HashMap::new(),
        &session_start_mcp_tools,
        /*connectors*/ None,
        &[],
        &config,
        &session_start_mcp_servers,
        /*search_tool_enabled*/ false,
    );

    assert!(exposure.direct_tools.contains_key("mcp__rmcp__tool"));
    assert!(exposure.deferred_tools.is_none());
}

#[tokio::test]
async fn explicit_apps_selection_controls_direct_app_exposure() {
    let config = test_config().await;
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
        /*search_tool_enabled*/ false,
    );

    assert_eq!(
        exposure
            .direct_tools
            .keys()
            .cloned()
            .collect::<HashSet<_>>(),
        HashSet::from([
            "mcp__rmcp__tool".to_string(),
            "mcp__codex_apps__calendar_create_event".to_string(),
        ])
    );
    assert!(exposure.deferred_tools.is_none());
}
