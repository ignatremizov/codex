use std::collections::HashMap;
use std::collections::HashSet;

use codex_features::Feature;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::EffectiveMcpServer;
use codex_mcp::ToolInfo as McpToolInfo;

use crate::config::Config;
use crate::connectors;

pub(crate) const DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD: usize = 100;

pub(crate) struct McpToolExposure {
    pub(crate) direct_tools: HashMap<String, McpToolInfo>,
    pub(crate) deferred_tools: Option<HashMap<String, McpToolInfo>>,
}

pub(crate) fn build_mcp_tool_exposure(
    all_mcp_tools: &HashMap<String, McpToolInfo>,
    session_start_mcp_tools: &HashMap<String, McpToolInfo>,
    connectors: Option<&[connectors::AppInfo]>,
    explicitly_enabled_connectors: &[connectors::AppInfo],
    config: &Config,
    effective_mcp_servers: &HashMap<String, EffectiveMcpServer>,
    search_tool_enabled: bool,
) -> McpToolExposure {
    // `allow_implicit_invocation = false` only suppresses default direct/model-visible MCP
    // exposure. Hidden servers remain connected at runtime and intentionally stay discoverable
    // through deferred `tool_search` exposure.
    let direct_visible_mcp_tools = session_start_mcp_tools
        .iter()
        .filter(|(_, tool)| {
            effective_mcp_servers
                .get(tool.server_name.as_str())
                .is_some_and(|server| server.enabled() && server.allow_implicit_invocation())
        })
        .map(|(name, tool)| (name.clone(), tool.clone()))
        .collect::<HashMap<_, _>>();
    let mut deferred_tools = filter_non_codex_apps_mcp_tools_only(all_mcp_tools);
    if let Some(connectors) = connectors {
        deferred_tools.extend(filter_codex_apps_mcp_tools(
            all_mcp_tools,
            connectors,
            config,
        ));
    }
    let direct_tools_for_current_connector_selection = build_direct_tools(
        &direct_visible_mcp_tools,
        all_mcp_tools,
        connectors.unwrap_or(explicitly_enabled_connectors),
        config,
    );
    let mut direct_visible_defer_candidates =
        filter_non_codex_apps_mcp_tools_only(&direct_visible_mcp_tools);
    if let Some(connectors) = connectors {
        direct_visible_defer_candidates.extend(filter_codex_apps_mcp_tools(
            all_mcp_tools,
            connectors,
            config,
        ));
    }

    let should_defer = search_tool_enabled
        && (config
            .features
            .enabled(Feature::ToolSearchAlwaysDeferMcpTools)
            || direct_visible_defer_candidates.len() >= DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD);

    if !should_defer {
        let direct_tools = direct_tools_for_current_connector_selection;
        remove_direct_tools_from_deferred(&mut deferred_tools, direct_tools.keys());
        return McpToolExposure {
            direct_tools,
            // Keep all non-direct live tools registered/callable even when tool_search is off:
            // hidden-by-default servers and servers added after session start are runtime state,
            // not default direct context.
            deferred_tools: (!deferred_tools.is_empty()).then_some(deferred_tools),
        };
    }

    let direct_tools =
        filter_codex_apps_mcp_tools(all_mcp_tools, explicitly_enabled_connectors, config);
    remove_direct_tools_from_deferred(&mut deferred_tools, direct_tools.keys());

    McpToolExposure {
        direct_tools,
        deferred_tools: (!deferred_tools.is_empty()).then_some(deferred_tools),
    }
}

fn build_direct_tools(
    direct_visible_mcp_tools: &HashMap<String, McpToolInfo>,
    all_mcp_tools: &HashMap<String, McpToolInfo>,
    direct_codex_apps_connectors: &[connectors::AppInfo],
    config: &Config,
) -> HashMap<String, McpToolInfo> {
    let mut direct_tools = filter_non_codex_apps_mcp_tools_only(direct_visible_mcp_tools);
    direct_tools.extend(filter_codex_apps_mcp_tools(
        all_mcp_tools,
        direct_codex_apps_connectors,
        config,
    ));
    direct_tools
}

fn remove_direct_tools_from_deferred<'a>(
    deferred_tools: &mut HashMap<String, McpToolInfo>,
    direct_tool_names: impl Iterator<Item = &'a String>,
) {
    for direct_tool_name in direct_tool_names {
        deferred_tools.remove(direct_tool_name);
    }
}

fn filter_non_codex_apps_mcp_tools_only(
    mcp_tools: &HashMap<String, McpToolInfo>,
) -> HashMap<String, McpToolInfo> {
    mcp_tools
        .iter()
        .filter(|(_, tool)| tool.server_name != CODEX_APPS_MCP_SERVER_NAME)
        .map(|(name, tool)| (name.clone(), tool.clone()))
        .collect()
}

fn filter_codex_apps_mcp_tools(
    mcp_tools: &HashMap<String, McpToolInfo>,
    connectors: &[connectors::AppInfo],
    config: &Config,
) -> HashMap<String, McpToolInfo> {
    let allowed: HashSet<&str> = connectors
        .iter()
        .map(|connector| connector.id.as_str())
        .collect();

    mcp_tools
        .iter()
        .filter(|(_, tool)| {
            if tool.server_name != CODEX_APPS_MCP_SERVER_NAME {
                return false;
            }
            let Some(connector_id) = tool.connector_id.as_deref() else {
                return false;
            };
            allowed.contains(connector_id) && connectors::codex_app_tool_is_enabled(config, tool)
        })
        .map(|(name, tool)| (name.clone(), tool.clone()))
        .collect()
}

#[cfg(test)]
#[path = "mcp_tool_exposure_test.rs"]
mod tests;
