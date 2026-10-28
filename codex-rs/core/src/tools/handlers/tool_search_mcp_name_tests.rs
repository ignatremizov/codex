//! Exact names distinguish tools whose legacy flattened identifiers collide.

use super::*;
use crate::tools::handlers::McpHandler;
use codex_mcp::ToolInfo;
use pretty_assertions::assert_eq;

#[test]
fn advertised_mcp_names_disambiguate_legacy_flattened_collisions() {
    let entries = [("ab", "c"), ("a", "bc")]
        .into_iter()
        .map(|(server, name)| {
            let info = ToolInfo {
                server_name: server.to_string(),
                supports_parallel_tool_calls: false,
                server_origin: None,
                callable_name: name.to_string(),
                callable_namespace: format!("mcp__{server}"),
                namespace_description: Some("Custom provider".to_string()),
                tool: rmcp::model::Tool::new(
                    name.to_string(),
                    "Shared description",
                    Arc::new(rmcp::model::object(serde_json::json!({
                        "type": "object",
                        "properties": {},
                    }))),
                ),
                openai_file_input_optional_fields: Default::default(),
                connector_id: None,
                connector_name: None,
                plugin_display_names: Vec::new(),
            };
            McpHandler::new(info)
                .expect("MCP tool should build")
                .search_info()
                .expect("MCP search metadata")
        })
        .collect::<Vec<_>>();
    let expected = entries
        .iter()
        .map(|info| info.entry.to_loadable_spec())
        .collect::<Vec<_>>();
    let handler = ToolSearchHandler::new(entries, ToolSearchSourceListing::Include);
    assert_eq!(
        handler.search("mcp__ab__c", /*limit*/ 1),
        vec![expected[0].clone()]
    );
    assert_eq!(
        handler.search("mcp__a__bc", /*limit*/ 1),
        vec![expected[1].clone()]
    );
}
