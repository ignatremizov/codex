use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

use codex_connectors::AppToolPolicyEvaluator;
use codex_connectors::AppToolPolicyInput;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::EffectiveMcpServer;
use codex_mcp::McpBinding;
use codex_mcp::ToolInfo as McpToolInfo;
use codex_mcp::tool_is_model_visible;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use tracing::instrument;
use tracing::warn;

use crate::config::Config;
use crate::connectors;
use crate::tools::handlers::McpHandler;
use crate::tools::registry::ToolRegistry;

const MAX_AGENT_PLUGIN_MCP_SPEC_BYTES: usize = 8_000;
const MAX_AGENT_PLUGIN_MCP_TOTAL_BYTES: usize = 64_000;

#[derive(Default)]
pub(crate) struct McpHandlerCache {
    cached: Mutex<Option<CachedMcpHandlers>>,
}

struct CachedMcpHandlers {
    binding: Weak<McpBinding>,
    handlers: HashMap<ToolName, Arc<McpHandler>>,
}

pub(crate) struct McpToolRegistrationContext<'a> {
    pub(crate) session_start_mcp_servers: &'a HashMap<String, EffectiveMcpServer>,
    pub(crate) config: &'a Config,
    pub(crate) apps_enabled: bool,
    pub(crate) mcp_server_catalog: &'a codex_mcp::ResolvedMcpCatalog,
    pub(crate) search_tool_enabled: bool,
}

impl McpHandlerCache {
    pub(crate) fn append_mcp_tools(
        &self,
        binding: &Arc<McpBinding>,
        stable_direct_tools: &HashMap<String, McpToolInfo>,
        context: McpToolRegistrationContext<'_>,
        registry: &mut ToolRegistry,
    ) -> HashSet<ToolName> {
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !cached
            .as_ref()
            .and_then(|cached| cached.binding.upgrade())
            .is_some_and(|cached_binding| Arc::ptr_eq(&cached_binding, binding))
        {
            *cached = None;
        }

        let cached = cached.get_or_insert_with(|| CachedMcpHandlers {
            binding: Arc::downgrade(binding),
            handlers: HashMap::new(),
        });
        let current_tool_names = binding
            .tools()
            .iter()
            .map(McpToolInfo::canonical_tool_name)
            .collect::<HashSet<_>>();
        let mut all_mcp_tools = binding.tools().to_vec();
        all_mcp_tools.extend(
            stable_direct_tools
                .values()
                .filter(|tool| !current_tool_names.contains(&tool.canonical_tool_name()))
                .cloned(),
        );
        append_mcp_tools(&all_mcp_tools, context, &mut cached.handlers, registry)
    }
}

#[instrument(level = "trace", skip_all)]
fn append_mcp_tools(
    all_mcp_tools: &[McpToolInfo],
    context: McpToolRegistrationContext<'_>,
    handlers: &mut HashMap<ToolName, Arc<McpHandler>>,
    registry: &mut ToolRegistry,
) -> HashSet<ToolName> {
    let McpToolRegistrationContext {
        session_start_mcp_servers,
        config,
        apps_enabled,
        mcp_server_catalog,
        search_tool_enabled,
    } = context;
    // Keep regular MCP tools first; Apps tools also require connector and policy checks.
    let non_app_tools = filter_non_codex_apps_mcp_tools_only(all_mcp_tools);
    let app_tools = apps_enabled
        .then(|| filter_codex_apps_mcp_tools(all_mcp_tools, config))
        .into_iter()
        .flatten();
    let exposure = if search_tool_enabled {
        ToolExposure::Deferred
    } else {
        ToolExposure::Direct
    };
    let mut registered_tools = HashSet::new();
    let mut agent_plugin_bytes = 0usize;
    for tool in non_app_tools.chain(app_tools) {
        let tool_name = tool.canonical_tool_name();
        let agent_plugin = mcp_server_catalog
            .server(&tool.server_name)
            .map(|server| server.source().is_agent_plugin())
            .or_else(|| {
                session_start_mcp_servers
                    .get(&tool.server_name)
                    .map(EffectiveMcpServer::is_agent_plugin)
            })
            .unwrap_or(false);
        let handler = match handlers.entry(tool_name.clone()) {
            Entry::Occupied(entry) => Arc::clone(entry.get()),
            Entry::Vacant(entry) => {
                let handler = if agent_plugin {
                    McpHandler::new_agent_plugin(tool.clone())
                } else {
                    McpHandler::new(tool.clone())
                };

                match handler {
                    Ok(handler) => Arc::clone(entry.insert(Arc::new(handler))),
                    Err(err) => {
                        warn!("Skipping MCP tool `{tool_name}`: failed to build tool spec: {err}");
                        continue;
                    }
                }
            }
        };

        let fits_agent_budget = if agent_plugin {
            handler.model_spec_bytes().is_ok_and(|bytes| {
                if bytes > MAX_AGENT_PLUGIN_MCP_SPEC_BYTES {
                    return false;
                }
                let next = agent_plugin_bytes.saturating_add(bytes);
                if next <= MAX_AGENT_PLUGIN_MCP_TOTAL_BYTES {
                    agent_plugin_bytes = next;
                    true
                } else {
                    false
                }
            })
        } else {
            true
        };
        let tool_exposure = if fits_agent_budget {
            exposure
        } else {
            ToolExposure::Hidden
        };
        if registry.register_external_with_exposure(handler, tool_exposure) && fits_agent_budget {
            registered_tools.insert(tool_name);
        }
    }
    registered_tools
}

pub(crate) struct McpToolExposure {
    pub(crate) direct_tools: HashMap<String, McpToolInfo>,
    pub(crate) deferred_tools: Option<HashMap<String, McpToolInfo>>,
}

#[instrument(level = "trace", skip_all)]
pub(crate) fn build_mcp_tool_exposure(
    all_mcp_tools: &HashMap<String, McpToolInfo>,
    session_start_mcp_tools: &HashMap<String, McpToolInfo>,
    connectors: Option<&[connectors::AppInfo]>,
    explicitly_enabled_connectors: &[connectors::AppInfo],
    config: &Config,
    effective_mcp_servers: &HashMap<String, EffectiveMcpServer>,
    search_tool_enabled: bool,
) -> McpToolExposure {
    // Hidden-by-default servers remain connected and discoverable through deferred exposure.
    let direct_visible_mcp_tools = session_start_mcp_tools
        .iter()
        .filter(|(_, tool)| {
            effective_mcp_servers
                .get(tool.server_name.as_str())
                .is_some_and(|server| server.enabled() && server.config().allow_implicit_invocation)
        })
        .map(|(name, tool)| (name.clone(), tool.clone()))
        .collect::<HashMap<_, _>>();
    let mut deferred_tools = collect_non_app_mcp_tools(all_mcp_tools);
    if let Some(connectors) = connectors {
        deferred_tools.extend(collect_app_mcp_tools(all_mcp_tools, connectors, config));
    }
    let direct_connectors = connectors.unwrap_or(explicitly_enabled_connectors);
    let mut direct_tools = collect_non_app_mcp_tools(&direct_visible_mcp_tools);
    direct_tools.extend(collect_app_mcp_tools(
        all_mcp_tools,
        direct_connectors,
        config,
    ));
    if search_tool_enabled {
        direct_tools = collect_app_mcp_tools(all_mcp_tools, explicitly_enabled_connectors, config);
    }
    for direct_tool_name in direct_tools.keys() {
        deferred_tools.remove(direct_tool_name);
    }

    McpToolExposure {
        direct_tools,
        deferred_tools: (!deferred_tools.is_empty()).then_some(deferred_tools),
    }
}

fn filter_non_codex_apps_mcp_tools_only(
    mcp_tools: &[McpToolInfo],
) -> impl Iterator<Item = &McpToolInfo> + '_ {
    mcp_tools.iter().filter(|tool| {
        tool.server_name != CODEX_APPS_MCP_SERVER_NAME && tool_is_model_visible(tool)
    })
}

fn filter_codex_apps_mcp_tools<'a>(
    mcp_tools: &'a [McpToolInfo],
    config: &'a Config,
) -> impl Iterator<Item = &'a McpToolInfo> + 'a {
    let app_tool_policy = AppToolPolicyEvaluator::new(&config.config_layer_stack);

    mcp_tools.iter().filter(move |tool| {
        if tool.server_name != CODEX_APPS_MCP_SERVER_NAME {
            return false;
        }
        if !tool_is_model_visible(tool) {
            return false;
        }
        let Some(connector_id) = tool.connector_id.as_deref() else {
            return false;
        };
        let annotations = tool.tool.annotations.as_ref();
        app_tool_policy
            .policy(AppToolPolicyInput {
                connector_id: Some(connector_id),
                link_id: None,
                tool_name: &tool.tool.name,
                tool_title: tool.tool.title.as_deref(),
                destructive_hint: annotations.and_then(|annotations| annotations.destructive_hint),
                open_world_hint: annotations.and_then(|annotations| annotations.open_world_hint),
            })
            .enabled
    })
}

fn collect_non_app_mcp_tools(
    mcp_tools: &HashMap<String, McpToolInfo>,
) -> HashMap<String, McpToolInfo> {
    mcp_tools
        .iter()
        .filter(|(_, tool)| {
            tool.server_name != CODEX_APPS_MCP_SERVER_NAME && tool_is_model_visible(tool)
        })
        .map(|(name, tool)| (name.clone(), tool.clone()))
        .collect()
}

fn collect_app_mcp_tools(
    mcp_tools: &HashMap<String, McpToolInfo>,
    connectors: &[connectors::AppInfo],
    config: &Config,
) -> HashMap<String, McpToolInfo> {
    let allowed = connectors
        .iter()
        .map(|connector| connector.id.as_str())
        .collect::<HashSet<_>>();
    let app_tool_policy = AppToolPolicyEvaluator::new(&config.config_layer_stack);

    mcp_tools
        .iter()
        .filter(|(_, tool)| {
            if tool.server_name != CODEX_APPS_MCP_SERVER_NAME || !tool_is_model_visible(tool) {
                return false;
            }
            let Some(connector_id) = tool.connector_id.as_deref() else {
                return false;
            };
            let annotations = tool.tool.annotations.as_ref();
            allowed.contains(connector_id)
                && app_tool_policy
                    .policy(AppToolPolicyInput {
                        connector_id: Some(connector_id),
                        link_id: None,
                        tool_name: &tool.tool.name,
                        tool_title: tool.tool.title.as_deref(),
                        destructive_hint: annotations
                            .and_then(|annotations| annotations.destructive_hint),
                        open_world_hint: annotations
                            .and_then(|annotations| annotations.open_world_hint),
                    })
                    .enabled
        })
        .map(|(name, tool)| (name.clone(), tool.clone()))
        .collect()
}

#[cfg(test)]
#[path = "mcp_tool_exposure_test.rs"]
mod tests;
