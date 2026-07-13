//! User-invoked MCP context and the stable, non-Apps prompt contract.
//!
//! Capturing a step only proposes exposure. The first sampling request freezes
//! the declarations it actually advertises; prewarm, status and idle activation
//! cannot freeze them. These snapshots are prompt data, never call authorization.

use super::TurnInput;
use super::session::Session;
use super::step_context::StepContext;
use crate::config::Config;
use crate::context::ContextualUserFragment;
use crate::context::McpServerUseInstructions;
use crate::mcp_tool_exposure::McpToolExposure;
use crate::mcp_tool_exposure::build_mcp_tool_exposure;
use crate::state::ActiveTurn;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::EffectiveMcpServer;
use codex_mcp::McpBinding;
use codex_mcp::ToolInfo;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolSpec;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::Mutex as AsyncMutex;

pub(crate) struct McpPromptState {
    pub(crate) startup_servers: HashMap<String, EffectiveMcpServer>,
    direct_tools: Mutex<Option<HashMap<String, ToolInfo>>>,
    pub(super) first_turn_servers: AsyncMutex<Vec<String>>,
}

impl McpPromptState {
    pub(super) fn new(startup_servers: HashMap<String, EffectiveMcpServer>) -> Self {
        Self {
            startup_servers,
            direct_tools: Mutex::new(None),
            first_turn_servers: AsyncMutex::new(Vec::new()),
        }
    }

    pub(crate) fn exposure(
        &self,
        binding: &McpBinding,
        config: &Config,
        apps_enabled: bool,
        selected_connectors: &HashSet<String>,
        search_enabled: bool,
    ) -> McpToolExposure {
        let tools = binding
            .tools()
            .iter()
            .map(|tool| (tool.canonical_tool_name().to_string(), tool.clone()))
            .collect::<HashMap<_, _>>();
        let startup_tools = tools
            .iter()
            .filter(|(_, tool)| {
                self.startup_servers
                    .get(&tool.server_name)
                    .is_some_and(|server| {
                        server.enabled() && server.config().allow_implicit_invocation
                    })
            })
            .map(|(name, tool)| (name.clone(), tool.clone()))
            .collect();
        let connectors = apps_enabled
            .then(|| crate::connectors::accessible_connectors_from_mcp_tools(binding.tools()));
        let selected = connectors
            .iter()
            .flatten()
            .filter(|connector| selected_connectors.contains(&connector.id))
            .cloned()
            .collect::<Vec<_>>();
        let mut exposure = build_mcp_tool_exposure(
            &tools,
            &startup_tools,
            connectors.as_deref(),
            &selected,
            config,
            &self.startup_servers,
            search_enabled,
        );
        if let Some(frozen) = self
            .direct_tools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            exposure
                .direct_tools
                .retain(|_, tool| tool.server_name == CODEX_APPS_MCP_SERVER_NAME);
            exposure.direct_tools.extend(frozen.clone());
            exposure.allow_direct_fallback = false;
        }
        if let Some(deferred) = exposure.deferred_tools.as_mut() {
            deferred.retain(|name, _| !exposure.direct_tools.contains_key(name));
        }
        exposure
    }

    pub(super) fn freeze(&self, tools: &[ToolInfo], specs: &[ToolSpec]) {
        let mut frozen = self
            .direct_tools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if frozen.is_some() {
            return;
        }
        *frozen = Some(direct_inventory(tools, specs));
    }
}

fn direct_inventory(tools: &[ToolInfo], specs: &[ToolSpec]) -> HashMap<String, ToolInfo> {
    tools
        .iter()
        .filter(|tool| {
            tool.server_name != CODEX_APPS_MCP_SERVER_NAME
                && specs.iter().any(|spec| {
                    let name = tool.canonical_tool_name().with_default_namespace();
                    match spec {
                        ToolSpec::Function(function) => {
                            name.namespace.as_deref()
                                == Some(codex_protocol::DEFAULT_FUNCTION_NAMESPACE)
                                && function.name == name.name
                        }
                        ToolSpec::Namespace(namespace) => {
                            name.namespace.as_deref() == Some(namespace.name.as_str())
                                && namespace.tools.iter().any(|tool| match tool {
                                    ResponsesApiNamespaceTool::Function(function) => {
                                        function.name == name.name
                                    }
                                    ResponsesApiNamespaceTool::Custom(_) => false,
                                })
                        }
                        ToolSpec::Freeform(_)
                        | ToolSpec::ToolSearch { .. }
                        | ToolSpec::WebSearch { .. } => false,
                    }
                })
        })
        .map(|tool| (tool.canonical_tool_name().to_string(), tool.clone()))
        .collect()
}

pub(crate) fn is_mcp_use_input(input: &TurnInput) -> bool {
    matches!(input, TurnInput::ResponseItem(envelope)
        if McpServerUseInstructions::matches_response_item(&envelope.item))
}

fn use_text(item: &ResponseItem, server_name: &str) -> Option<String> {
    let ResponseItem::Message { role, content, .. } = item else {
        return None;
    };
    if role != "developer" {
        return None;
    }
    content.iter().find_map(|content| match content {
        ContentItem::InputText { text }
            if McpServerUseInstructions::parse_server_name(text).as_deref()
                == Some(server_name) =>
        {
            Some(text.clone())
        }
        ContentItem::InputText { .. }
        | ContentItem::OutputText { .. }
        | ContentItem::InputImage { .. }
        | ContentItem::InputAudio { .. } => None,
    })
}

fn render_inventory(server_name: &str, tools: &[ToolInfo]) -> ResponseItemEnvelope {
    let mut tools = tools
        .iter()
        .filter(|tool| tool.server_name == server_name)
        .map(|tool| (tool.canonical_tool_name().to_string(), tool))
        .collect::<Vec<_>>();
    tools.sort_by(|(left, _), (right, _)| left.cmp(right));
    let inventory = tools
        .into_iter()
        .map(|(name, tool)| serde_json::json!({"canonical_tool_name": name, "tool_info": tool}))
        .collect::<Vec<_>>();
    let fragment = McpServerUseInstructions::new(
        server_name.to_string(),
        serde_json::to_string_pretty(&inventory).unwrap_or_else(|_| "[]".to_string()),
    );
    ResponseItemEnvelope::new(ContextualUserFragment::into(fragment))
}

impl Session {
    pub(crate) async fn latest_mcp_server_use_context_text(
        &self,
        server_name: &str,
    ) -> Option<String> {
        let queued = self.input_queue.queued_turn_inputs().await;
        let mut pending = queued;
        let active_state = self
            .active_turn
            .lock()
            .await
            .as_ref()
            .map(|active| Arc::clone(&active.turn_state));
        if let Some(state) = active_state {
            pending.extend(state.lock().await.pending_input.as_slice().iter().cloned());
        }
        if let Some(text) = pending.iter().rev().find_map(|input| match input {
            TurnInput::ResponseItem(envelope) => use_text(&envelope.item, server_name),
            TurnInput::UserInput { .. }
            | TurnInput::FunctionCallOutput(_)
            | TurnInput::InterAgentCommunication(_) => None,
        }) {
            return Some(text);
        }
        self.clone_history()
            .await
            .raw_items()
            .rev()
            .find_map(|item| use_text(item, server_name))
    }

    pub(crate) async fn mcp_server_would_be_direct_at_session_start(
        self: &Arc<Self>,
        server_name: &str,
    ) -> bool {
        self.refresh_mcp_if_dirty().await;
        let Some(binding) = self
            .services
            .mcp_runtime
            .current_binding_for_call(server_name)
            .await
        else {
            return false;
        };
        let frozen = self
            .mcp_prompt
            .direct_tools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let direct = match frozen {
            Some(direct) => direct,
            // No declaration has been advertised yet. Status and activation
            // must not materialize or freeze a speculative prompt contract.
            None => return false,
        };
        let current = binding
            .tools()
            .iter()
            .filter(|tool| tool.server_name == server_name)
            .map(|tool| (tool.canonical_tool_name().to_string(), tool))
            .collect::<HashMap<_, _>>();
        let direct = direct
            .iter()
            .filter(|(_, tool)| tool.server_name == server_name)
            .map(|(name, tool)| (name.clone(), tool))
            .collect::<HashMap<_, _>>();
        !current.is_empty() && current == direct
    }

    pub(crate) async fn activate_mcp_server(self: &Arc<Self>, server_name: String) {
        if self
            .mcp_server_would_be_direct_at_session_start(&server_name)
            .await
        {
            return;
        }
        if self.reference_context_item().await.is_none() {
            let mut queued = self.mcp_prompt.first_turn_servers.lock().await;
            if !queued.contains(&server_name) {
                queued.push(server_name);
            }
            return;
        }
        let binding = self
            .services
            .mcp_runtime
            .current_binding_for_call(&server_name)
            .await;
        let item = render_inventory(
            &server_name,
            binding
                .as_deref()
                .map(McpBinding::tools)
                .unwrap_or_default(),
        );
        if self.latest_mcp_server_use_context_text(&server_name).await
            == use_text(&item.item, &server_name)
        {
            return;
        }
        let Err(items) = self
            .inject_hook_context_if_running(vec![item.item.clone()])
            .await
        else {
            return;
        };
        let turn = self.new_default_turn().await;
        self.record_mcp_use_items(
            turn.model_info(),
            items.into_iter().map(ResponseItemEnvelope::new).collect(),
        )
        .await;
    }

    pub(crate) async fn record_queued_mcp_use(self: &Arc<Self>, step: &StepContext) {
        // Keep admitted names until recording completes. Cancellation at a history
        // or persistence await must leave them available to the next real turn.
        let names = self.mcp_prompt.first_turn_servers.lock().await.clone();
        let mut items = Vec::new();
        let direct = direct_inventory(step.mcp.tools(), &step.tool_router.model_visible_specs());
        for name in &names {
            let tools = step
                .mcp
                .tools()
                .iter()
                .filter(|tool| &tool.server_name == name)
                .collect::<Vec<_>>();
            if !tools.is_empty()
                && tools
                    .iter()
                    .all(|tool| direct.contains_key(&tool.canonical_tool_name().to_string()))
            {
                continue;
            }
            let item = render_inventory(name, step.mcp.tools());
            if self.latest_mcp_server_use_context_text(name).await != use_text(&item.item, name) {
                items.push(item);
            }
        }
        self.record_mcp_use_items(&step.settings.model_info, items)
            .await;
        self.mcp_prompt
            .first_turn_servers
            .lock()
            .await
            .retain(|name| !names.contains(name));
    }

    pub(crate) async fn record_mcp_use_items(
        &self,
        model_info: &ModelInfo,
        mut items: Vec<ResponseItemEnvelope>,
    ) {
        // Accepted publication is independently driven. An empty retry/abort still
        // waits for the same worker; it never recreates a potentially committed append.
        let permit = super::thread_settings::acquire_persistence_lock(self).await;
        // A cancelled caller may already have dispatched this un-stamped queued
        // inventory. Recheck after the preceding worker's installation.
        {
            let state = self.state.lock().await;
            items.retain(|item| {
                if item.metadata.is_some() || item.item.id().is_some() {
                    return true;
                }
                let ResponseItem::Message { content, .. } = &item.item else {
                    return true;
                };
                let server = content.iter().find_map(|content| match content {
                    ContentItem::InputText { text } => {
                        McpServerUseInstructions::parse_server_name(text)
                    }
                    ContentItem::OutputText { .. }
                    | ContentItem::InputImage { .. }
                    | ContentItem::InputAudio { .. } => None,
                });
                let Some(server) = server else {
                    return true;
                };
                // Only the latest inventory for this server can satisfy a retry.
                // Reverting A -> B -> A must preserve the final accepted A.
                state
                    .history
                    .annotated_items()
                    .iter()
                    .rev()
                    .find(|previous| use_text(&previous.item, &server).is_some())
                    != Some(item)
            });
        }
        if items.is_empty() {
            return;
        }
        let rollout_items = items
            .iter()
            .cloned()
            .map(RolloutItem::ResponseItem)
            .collect();
        let policy = model_info.truncation_policy.into();
        let result = match self.dispatch_history_publication(
            permit,
            rollout_items,
            Vec::new(),
            /*acknowledgement*/ None,
            move |state| state.history.record_annotated_items(&items, policy),
        ) {
            Ok(receiver) => self.publication_result(receiver).await,
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            tracing::error!("failed to publish MCP use context: {error}");
        }
    }

    pub(crate) async fn record_active_mcp_use_before_abort(self: &Arc<Self>, active: &ActiveTurn) {
        let (items, step) = {
            let mut state = active.turn_state.lock().await;
            let pending = state.pending_input.take();
            let (explicit, rest): (Vec<_>, Vec<_>) =
                pending.into_iter().partition(is_mcp_use_input);
            state.pending_input.append_to_front(rest);
            (explicit, state.last_known_step_context.clone())
        };
        let items = items
            .into_iter()
            .filter_map(|input| match input {
                TurnInput::ResponseItem(item) => Some(item),
                TurnInput::UserInput { .. }
                | TurnInput::FunctionCallOutput(_)
                | TurnInput::InterAgentCommunication(_) => None,
            })
            .collect();
        if let Some(step) = step {
            self.record_mcp_use_items(&step.settings.model_info, items)
                .await;
        } else {
            let turn = self.new_default_turn().await;
            self.record_mcp_use_items(turn.model_info(), items).await;
        }
    }

    pub(crate) async fn active_turn_has_pending_mcp_server_use_boundary(&self) -> bool {
        let state = self
            .active_turn
            .lock()
            .await
            .as_ref()
            .map(|active| Arc::clone(&active.turn_state));
        let Some(state) = state else {
            return false;
        };
        state
            .lock()
            .await
            .pending_input
            .as_slice()
            .iter()
            .any(is_mcp_use_input)
    }
}

#[cfg(test)]
#[path = "mcp_prompt_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "mcp_prompt_persistence_tests.rs"]
mod persistence_tests;
