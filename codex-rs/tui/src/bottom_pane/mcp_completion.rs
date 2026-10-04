//! MCP argument completion preserves the entire server name, including spaces.

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum McpCompletion {
    Use,
    Verbose,
    Server(String),
}

impl McpCompletion {
    pub(super) fn text(&self) -> String {
        match self {
            Self::Use => "/mcp use ".into(),
            Self::Verbose => "/mcp verbose".into(),
            Self::Server(name) => format!("/mcp use {name}"),
        }
    }

    pub(super) fn description(&self) -> &'static str {
        match self {
            Self::Use => "request a server's tools in context",
            Self::Verbose => "show full MCP inventory",
            Self::Server(_) => "request explicit MCP context",
        }
    }
}

pub(super) fn candidates(text: &str, names: &[String]) -> Option<Vec<McpCompletion>> {
    let tail = text.strip_prefix("/mcp")?;
    if tail.is_empty() || !tail.starts_with(char::is_whitespace) {
        return None;
    }
    let args = tail.trim_start();
    if let Some((verb, name)) = args.split_once(char::is_whitespace)
        && verb.eq_ignore_ascii_case("use")
    {
        let prefix = name.trim_start().to_lowercase();
        return Some(
            names
                .iter()
                .filter(|name| name.to_lowercase().starts_with(&prefix))
                .cloned()
                .map(McpCompletion::Server)
                .collect(),
        );
    }
    let prefix = args.to_ascii_lowercase();
    Some(
        [
            ("use", McpCompletion::Use),
            ("verbose", McpCompletion::Verbose),
        ]
        .into_iter()
        .filter(|(verb, _)| verb.starts_with(&prefix))
        .map(|(_, completion)| completion)
        .collect(),
    )
}

#[cfg(test)]
#[path = "mcp_completion_tests.rs"]
mod tests;
