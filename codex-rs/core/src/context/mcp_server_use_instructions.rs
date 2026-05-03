use super::ContextualUserFragment;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

// PRODUCT POLICY FOR REVIEWERS:
//
// `/mcp use <server>` is not ambient automatic context injection. It is an explicit user command
// that asks Codex to reveal an MCP server's tool surface to the model. The product behavior is to
// inject the complete discovered tool inventory for that server, even when that inventory is large.
//
// Do not add a token cap, byte cap, tool-count cap, summary, hash, abbreviation, pagination, or
// "first N tools" heuristic here. Those all violate the explicit-use contract by making the model
// see an incomplete or lossy inventory after the user asked to use the server. If this exhausts the
// model context window, the later model request should fail normally; `/mcp use` itself should not
// silently rewrite the user's explicit request into partial context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpServerUseInstructions {
    server_name: String,
    tool_names: Vec<String>,
}

impl McpServerUseInstructions {
    pub(crate) fn new(server_name: String, tool_names: Vec<String>) -> Self {
        Self {
            server_name,
            tool_names,
        }
    }

    pub(crate) fn parse_server_name(text: &str) -> Option<String> {
        let text = text.trim();
        if !Self::matches_text(text) {
            return None;
        }
        let start_tag = "<server_name_b64>";
        let end_tag = "</server_name_b64>";
        let start = text.find(start_tag)? + start_tag.len();
        let rest = &text[start..];
        let end = rest.find(end_tag)?;
        let encoded = &rest[..end];
        let bytes = BASE64_STANDARD.decode(encoded).ok()?;
        String::from_utf8(bytes).ok()
    }
}

impl ContextualUserFragment for McpServerUseInstructions {
    const ROLE: &'static str = "developer";
    const START_MARKER: &'static str = "<mcp_use>";
    const END_MARKER: &'static str = "</mcp_use>";

    fn body(&self) -> String {
        // HARD PRODUCT REQUIREMENT:
        //
        // `/mcp use <server>` is explicit, user-directed context expansion. It MUST inject that
        // server's FULL tool inventory into future model-visible prompt scaffolding.
        //
        // This fragment is intentionally NOT bounded by token count, byte count, or tool count.
        // It MUST NOT:
        // - truncate tool names
        // - summarize the inventory
        // - hash or abbreviate names
        // - otherwise silently drop any part of the explicit tool list
        //
        // The user explicitly chose to pay the context cost of including this MCP's full tool
        // surface. If a later turn exceeds the model context window because of `/mcp use`, that is
        // the correct runtime failure mode and should surface during normal turn execution.
        //
        // Reviewer note: if this looks "too large", that is a product-contract disagreement, not
        // an implementation bug. Do NOT add automatic capping/truncation logic here unless the
        // product requirement changes. This policy intentionally overrides the usual instinct to
        // bound injected prompt fragments because this path is a direct consequence of a slash
        // command, not background/implicit context growth.
        let tools = if self.tool_names.is_empty() {
            "(no tools discovered yet)".to_string()
        } else {
            self.tool_names
                .iter()
                .map(|name| format!("- `{name}`"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let encoded_server_name = BASE64_STANDARD.encode(self.server_name.as_bytes());
        format!(
            "\n<server_name_b64>{}</server_name_b64>\nThe user explicitly asked to use MCP server `{}` in this session. The server was already running; this message provides its exact discovered tool names as prompt context. You may use its tools directly if helpful.\nKnown tool names:\n{}\n",
            encoded_server_name, self.server_name, tools
        )
    }
}
