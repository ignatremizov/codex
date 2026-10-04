//! Resolve the model's Search profile on an already authorized Apps connection.
//! Keep the projected declaration and invocation pin private to its sampling step.

use super::McpBinding;
use crate::CODEX_APPS_MCP_SERVER_NAME;
use crate::MCP_TOOL_CODEX_APPS_META_KEY;
use crate::PreparedMcpCall;
use crate::ToolInfo;
use crate::pagination::MAX_CODEX_APPS_TOOL_CATALOG_ITEMS;
use crate::pagination::collect_paginated_with_limit;
use rmcp::model::RequestMetaObject;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use std::time::Duration;

pub const SEARCH_CONNECTOR: &str = "connector_openai_search_service";

pub fn is_model_search_tool(tool: &crate::ToolInfo) -> bool {
    tool.server_name == CODEX_APPS_MCP_SERVER_NAME
        && tool.connector_id.as_deref() == Some(SEARCH_CONNECTOR)
        && tool.tool.name.rsplit('.').next() == Some("web_run")
}

pub enum SearchDiscovery {
    Ready(Arc<McpBinding>),
    Critical(&'static str),
    Unavailable,
}

impl McpBinding {
    /// Preserve the discovery pin while letting normal dispatch recheck current tool visibility,
    /// account selection and approval policy. Never transfer the pin to a different transport.
    pub fn search_invocation_context(&self, live: &crate::PreparedMcpCall) -> Option<&JsonValue> {
        if !live.is_host_owned_apps() {
            return None;
        }
        let tool = live.tool_info();
        let scoped = self
            .calls
            .get(&(tool.server_name.clone(), tool.tool.name.to_string()))?;
        if !Arc::ptr_eq(&scoped.client.client, &live.client.client) {
            return None;
        }
        invocation_context(&scoped.tool_info().tool)
    }

    /// A catalog omission, policy denial, or untrusted registration never authorizes fallback.
    pub async fn discover_model_search(&self, turn_metadata: JsonValue) -> SearchDiscovery {
        let Some((index, original, call)) = self.model_search_entry() else {
            return SearchDiscovery::Unavailable;
        };
        let meta = RequestMetaObject::from(serde_json::Map::from_iter([(
            "x-codex-turn-metadata".to_string(),
            turn_metadata,
        )]));
        let client = &call.client;
        let limit = Duration::from_secs(5);
        let listed = collect_paginated_with_limit(
            "tools/list",
            /*overall_timeout*/ None,
            MAX_CODEX_APPS_TOOL_CATALOG_ITEMS,
            |params| {
                let mut params = params.unwrap_or_default();
                params.meta = Some(meta.clone());
                async move {
                    let page = client
                        .client
                        .list_tools_with_connector_ids(Some(params), /*timeout*/ None)
                        .await?;
                    Ok((page.tools, page.next_cursor))
                }
            },
        );
        let listed = tokio::time::timeout(limit, client.with_lease_request(listed)).await;
        let tools = match listed {
            Err(_) => return SearchDiscovery::Critical("discovery_timeout"),
            Ok(Err(err)) => {
                if matches!(
                    codex_rmcp_client::request_error_code(&err),
                    Some("transport_closed" | "request_timeout")
                ) || codex_rmcp_client::is_retryable_transport_error(&err)
                    || codex_rmcp_client::mcp_error(&err).is_some_and(|e| e.code.0 == -32603)
                {
                    return SearchDiscovery::Critical("discovery_error");
                }
                return SearchDiscovery::Unavailable;
            }
            Ok(Ok(tools)) => tools,
        };
        let Some(listed) = tools.into_iter().find(|tool| {
            tool.connector_id.as_deref() == Some(SEARCH_CONNECTOR)
                && tool.tool.name == original.tool.name
        }) else {
            return SearchDiscovery::Unavailable;
        };
        let mut projected = original.clone();
        projected.namespace_description = listed.connector_description;
        projected.tool = listed.tool;
        if !crate::tool_is_model_visible(&projected)
            || invocation_context(&projected.tool).is_none()
        {
            return SearchDiscovery::Unavailable;
        }
        SearchDiscovery::Ready(self.with_model_search_tool(index, call, projected))
    }

    /// Applies a scoped discovery to a fresh base when the search tool's configuration,
    /// transport, and source catalog revision are identical.
    pub fn reuse_model_search(
        &self,
        previous: &Self,
        discovery: &SearchDiscovery,
    ) -> Option<SearchDiscovery> {
        if !Arc::ptr_eq(&self.config, &previous.config) {
            return None;
        }
        let (index, tool, call) = self.model_search_entry()?;
        let (previous_index, previous_tool, previous_call) = previous.model_search_entry()?;
        // Catalog captures clone the managed wrapper while retaining its transport and catalog.
        if !Arc::ptr_eq(&call.client.client, &previous_call.client.client)
            || !Arc::ptr_eq(
                &call.client.tool_catalog,
                &previous_call.client.tool_catalog,
            )
            || call.catalog_snapshot.revision != previous_call.catalog_snapshot.revision
            || tool != previous_tool
        {
            return None;
        }
        Some(match discovery {
            SearchDiscovery::Ready(scoped) => SearchDiscovery::Ready(self.with_model_search_tool(
                index,
                call,
                scoped.tools.get(previous_index)?.clone(),
            )),
            SearchDiscovery::Critical(reason) => SearchDiscovery::Critical(reason),
            SearchDiscovery::Unavailable => SearchDiscovery::Unavailable,
        })
    }

    fn model_search_entry(&self) -> Option<(usize, &ToolInfo, &PreparedMcpCall)> {
        let (index, tool) = self
            .tools
            .iter()
            .enumerate()
            .find(|(_, tool)| is_model_search_tool(tool))?;
        let key = (tool.server_name.clone(), tool.tool.name.to_string());
        let call = self
            .calls
            .get(&key)
            .filter(|call| call.is_host_owned_apps())?;
        Some((index, tool, call))
    }

    fn with_model_search_tool(
        &self,
        index: usize,
        call: &PreparedMcpCall,
        tool: ToolInfo,
    ) -> Arc<Self> {
        let mut tools = self.tools.to_vec();
        let key = (tool.server_name.clone(), tool.tool.name.to_string());
        tools[index] = tool.clone();
        let mut calls = self.calls.clone();
        let mut call = call.clone();
        call.tool_info = tool.into();
        calls.insert(key, call);
        Arc::new(Self::new(
            Arc::clone(&self.connections),
            Arc::clone(&self.clients),
            Arc::clone(&self.config),
            self.plugins_available,
            tools,
            calls,
        ))
    }
}

// The connector owns the contents of its invocation context.
fn invocation_context(tool: &rmcp::model::Tool) -> Option<&JsonValue> {
    tool.meta
        .as_ref()?
        .get(MCP_TOOL_CODEX_APPS_META_KEY)?
        .get("invocation_context")
        .filter(|context| context.is_object())
}

#[cfg(test)]
#[path = "model_search_tests.rs"]
mod tests;
