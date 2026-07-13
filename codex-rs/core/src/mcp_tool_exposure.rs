use std::collections::HashMap;
use std::collections::HashSet;
use std::num::NonZeroUsize;
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
use crate::tools::handlers::McpHandlerDefinition;
use crate::tools::registry::ToolRegistry;

const MAX_AGENT_PLUGIN_MCP_SPEC_BYTES: usize = 8_000;
const MAX_AGENT_PLUGIN_MCP_TOTAL_BYTES: usize = 64_000;

#[derive(Default)]
pub(crate) struct McpHandlerCache {
    handlers: Mutex<HashMap<ToolName, CachedMcpHandler>>,
}

struct CachedMcpHandler {
    agent_plugin: bool,
    schema_max_bytes: Option<NonZeroUsize>,
    handler: Arc<McpHandler>,
}

/// Shares definitions for already-shared catalogs without keeping completed sessions alive.
#[derive(Default)]
pub(crate) struct SharedMcpDefinitions {
    definitions: Mutex<HashMap<McpDefinitionKey, Weak<McpHandlerDefinition>>>,
}

#[derive(Eq, Hash, PartialEq)]
struct McpDefinitionKey {
    input_schema: usize,
    tool_name: ToolName,
    agent_plugin: bool,
    schema_max_bytes: Option<NonZeroUsize>,
}

impl SharedMcpDefinitions {
    fn get_or_create(
        &self,
        tool: &McpToolInfo,
        agent_plugin: bool,
        schema_max_bytes: Option<NonZeroUsize>,
    ) -> Result<Arc<McpHandlerDefinition>, serde_json::Error> {
        let key = McpDefinitionKey {
            // A live definition owns this Arc. The address only selects a candidate;
            // complete prepared metadata equality is still required below.
            input_schema: Arc::as_ptr(&tool.tool.input_schema) as usize,
            tool_name: tool.canonical_tool_name(),
            agent_plugin,
            schema_max_bytes,
        };
        let budget = schema_max_bytes.map(NonZeroUsize::get);
        let existing = self
            .definitions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .and_then(Weak::upgrade);
        if let Some(existing) =
            existing.filter(|definition| definition.matches(tool, agent_plugin, budget))
        {
            return Ok(existing);
        }

        // Normalization can be expensive and must not hold the manager's index lock.
        let definition = Arc::new(McpHandlerDefinition::new(
            tool.clone(),
            agent_plugin,
            budget,
        )?);
        let mut definitions = self
            .definitions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = definitions.get(&key).and_then(Weak::upgrade);
        let winner = current
            .as_ref()
            .filter(|current| current.matches(tool, agent_plugin, budget))
            .cloned();
        if winner.is_none() {
            definitions.insert(key, Arc::downgrade(&definition));
        }
        // An upgraded or losing definition may own a large schema graph.
        drop(definitions);
        drop(current);
        Ok(winner.unwrap_or(definition))
    }
}

pub(crate) struct McpToolRegistrationContext<'a> {
    pub(crate) session_start_mcp_servers: &'a HashMap<String, EffectiveMcpServer>,
    pub(crate) config: &'a Config,
    pub(crate) apps_enabled: bool,
    pub(crate) mcp_server_catalog: &'a codex_mcp::ResolvedMcpCatalog,
    pub(crate) search_tool_enabled: bool,
}

impl McpHandlerCache {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn append_mcp_tools(
        &self,
        binding: &Arc<McpBinding>,
        stable_direct_tools: &HashMap<String, McpToolInfo>,
        context: McpToolRegistrationContext<'_>,
        shared_definitions: &SharedMcpDefinitions,
        registry: &mut ToolRegistry,
    ) -> HashSet<ToolName> {
        let mut handlers = self
            .handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut all_mcp_tools = binding
            .tools()
            .iter()
            .map(|tool| {
                stable_direct_tools
                    .get(&tool.canonical_tool_name().to_string())
                    .unwrap_or(tool)
                    .clone()
            })
            .collect::<Vec<_>>();
        let current_names = binding
            .tools()
            .iter()
            .map(|tool| tool.canonical_tool_name().to_string())
            .collect::<HashSet<_>>();
        let mut retired_direct = stable_direct_tools
            .iter()
            .filter(|(name, _)| !current_names.contains(*name))
            .collect::<Vec<_>>();
        retired_direct.sort_by_key(|(name, _)| *name);
        all_mcp_tools.extend(retired_direct.into_iter().map(|(_, tool)| tool.clone()));
        append_mcp_tools(
            &all_mcp_tools,
            context,
            &mut handlers,
            shared_definitions,
            registry,
        )
    }
}

#[instrument(level = "trace", skip_all)]
#[allow(clippy::too_many_arguments)]
fn append_mcp_tools(
    all_mcp_tools: &[McpToolInfo],
    context: McpToolRegistrationContext<'_>,
    handlers: &mut HashMap<ToolName, CachedMcpHandler>,
    shared_definitions: &SharedMcpDefinitions,
    registry: &mut ToolRegistry,
) -> HashSet<ToolName> {
    let McpToolRegistrationContext {
        session_start_mcp_servers,
        config,
        apps_enabled,
        mcp_server_catalog,
        search_tool_enabled,
    } = context;
    let current_tools: HashSet<_> = all_mcp_tools
        .iter()
        .map(McpToolInfo::canonical_tool_name)
        .collect();
    handlers.retain(|name, _| current_tools.contains(name));

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
    let mut pruned_shared_definitions = false;
    for tool in non_app_tools.chain(app_tools) {
        let tool_name = tool.canonical_tool_name();
        let server = mcp_server_catalog.server(&tool.server_name);
        let agent_plugin = server
            .map(|server| server.source().is_agent_plugin())
            .or_else(|| {
                session_start_mcp_servers
                    .get(&tool.server_name)
                    .map(EffectiveMcpServer::is_agent_plugin)
            })
            .unwrap_or(false);
        let server_config = server
            .map(codex_mcp::ResolvedMcpServer::config)
            .or_else(|| {
                session_start_mcp_servers
                    .get(&tool.server_name)
                    .map(EffectiveMcpServer::config)
            });
        let tool_input_schema_max_bytes =
            server_config.and_then(|config| config.tool_input_schema_max_bytes);
        // Handlers contain immutable tool metadata, not a connection or authorization snapshot.
        // Preserve their identity across equivalent bindings so the search index can also be reused.
        let handler = if let Some(cached) = handlers.get(&tool_name).filter(|cached| {
            cached.handler.tool_info() == tool
                && cached.agent_plugin == agent_plugin
                && cached.schema_max_bytes == tool_input_schema_max_bytes
        }) {
            Arc::clone(&cached.handler)
        } else {
            handlers.remove(&tool_name);
            if !pruned_shared_definitions {
                // Sweep once per miss-containing pass, never on the unchanged hot path.
                // Idle managers can retain dead keys/control blocks until their next miss.
                shared_definitions
                    .definitions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retain(|_, definition| definition.strong_count() > 0);
                pruned_shared_definitions = true;
            }
            let handler = shared_definitions
                .get_or_create(tool, agent_plugin, tool_input_schema_max_bytes)
                .map(McpHandler::from_definition);

            let handler = match handler {
                Ok(handler) => Arc::new(handler),
                Err(err) => {
                    warn!("Skipping MCP tool `{tool_name}`: failed to build tool spec: {err}");
                    continue;
                }
            };
            handlers.insert(
                tool_name.clone(),
                CachedMcpHandler {
                    agent_plugin,
                    schema_max_bytes: tool_input_schema_max_bytes,
                    handler: Arc::clone(&handler),
                },
            );
            handler
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
    pub(crate) allow_direct_fallback: bool,
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
    direct_tools.extend(
        deferred_tools
            .iter()
            .filter(|(_, tool)| {
                let implicit = tool.server_name == CODEX_APPS_MCP_SERVER_NAME
                    || effective_mcp_servers
                        .get(&tool.server_name)
                        .is_some_and(|server| {
                            server.enabled() && server.config().allow_implicit_invocation
                        });
                implicit
                    && tool
                        .canonical_tool_name()
                        .with_default_namespace()
                        .namespace
                        .as_ref()
                        .is_some_and(|namespace| {
                            config
                                .code_mode
                                .direct_only_tool_namespaces
                                .contains(namespace)
                        })
            })
            .map(|(name, tool)| (name.clone(), tool.clone())),
    );
    for direct_tool_name in direct_tools.keys() {
        deferred_tools.remove(direct_tool_name);
    }

    McpToolExposure {
        direct_tools,
        deferred_tools: (!deferred_tools.is_empty()).then_some(deferred_tools),
        allow_direct_fallback: true,
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
