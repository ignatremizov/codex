//! Width-independent display content retained after an MCP call completes.
//!
//! The wire result can contain multi-megabyte image, audio, or resource bodies. Validate each
//! block once with the MCP model, then retain only what history rendering actually displays.

use crate::text_formatting::format_json_compact;
use codex_protocol::mcp::CallToolResult;
use rmcp::model::ContentBlock;
use rmcp::model::ResourceContents;
use serde::Deserialize;
use std::borrow::Cow;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum McpResultKind {
    Standard,
    NodeRepl,
}

#[derive(Debug)]
pub(super) struct McpToolResult {
    pub(super) content: Vec<McpContentBlock>,
    pub(super) is_error: bool,
}

#[derive(Debug)]
pub(super) struct McpContentBlock {
    display: McpContentDisplay,
    pub(super) is_image: bool,
    /// Code mode uses a top-level `text` field even on malformed or non-text blocks.
    original_text: Option<String>,
}

#[derive(Debug)]
enum McpContentDisplay {
    Text(String),
    Summary(Cow<'static, str>),
    Json(String),
}

impl McpToolResult {
    /// Consumes a wire result, dropping bodies represented only by media or resource summaries.
    ///
    /// Canonical MCP deserialization preserves full text and the exact JSON fallback for malformed
    /// or unknown blocks. Media payloads are represented by markers without decoding their bodies.
    pub(super) fn new(result: CallToolResult, kind: McpResultKind) -> Self {
        let content = result
            .content
            .into_iter()
            .map(|block| {
                // Deserialize by reference so malformed blocks remain available for the exact
                // JSON fallback. Successful blocks no longer retain their wire representation.
                let parsed = ContentBlock::deserialize(&block);
                let is_image = matches!(&parsed, Ok(ContentBlock::Image(_)));
                let original_text = match (&parsed, kind) {
                    (Ok(ContentBlock::Text(_)), _) | (_, McpResultKind::Standard) => None,
                    (_, McpResultKind::NodeRepl) => block
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                };
                let display = match parsed {
                    Ok(ContentBlock::Text(text)) => McpContentDisplay::Text(text.text),
                    Ok(ContentBlock::Image(_)) => {
                        McpContentDisplay::Summary("Returned image".into())
                    }
                    Ok(ContentBlock::Audio(_)) => {
                        McpContentDisplay::Summary("<audio content>".into())
                    }
                    Ok(ContentBlock::Resource(resource)) => {
                        let summary = match resource.resource {
                            ResourceContents::TextResourceContents { uri, .. }
                            | ResourceContents::BlobResourceContents { uri, .. } => {
                                format!("embedded resource: {uri}").into()
                            }
                            _ => "<unknown embedded resource>".into(),
                        };
                        McpContentDisplay::Summary(summary)
                    }
                    Ok(ContentBlock::ResourceLink(link)) => {
                        McpContentDisplay::Summary(format!("link: {}", link.uri).into())
                    }
                    Ok(_) | Err(_) => McpContentDisplay::Json(block.to_string()),
                };
                McpContentBlock {
                    display,
                    is_image,
                    original_text,
                }
            })
            .collect();

        Self {
            content,
            is_error: result.is_error.unwrap_or(false),
        }
    }
}

impl McpContentBlock {
    /// Returns all retained text, with honest markers for media and embedded resource bodies.
    pub(super) fn render_full(&self) -> &str {
        if let Some(text) = &self.original_text {
            return text.trim_end_matches('\n');
        }
        match &self.display {
            McpContentDisplay::Text(text) | McpContentDisplay::Json(text) => {
                text.trim_end_matches('\n')
            }
            McpContentDisplay::Summary(summary) => summary,
        }
    }

    /// Returns the untruncated top-level text used by node_repl and cua_repl's compact and
    /// transcript views.
    ///
    /// Valid text blocks reuse their display storage. Only node_repl and cua_repl retain this extra
    /// field on malformed or non-text blocks, where it takes precedence over the usual display
    /// content.
    pub(super) fn text(&self) -> Option<&str> {
        match &self.display {
            McpContentDisplay::Text(text) => Some(text),
            McpContentDisplay::Summary(_) | McpContentDisplay::Json(_) => {
                self.original_text.as_deref()
            }
        }
    }

    /// Formats the complete text or fallback JSON. The caller applies a single preview limit
    /// across all blocks, while transcript and raw output retain the full result.
    pub(super) fn render(&self) -> Cow<'_, str> {
        match &self.display {
            McpContentDisplay::Text(text) | McpContentDisplay::Json(text) => {
                format_json_compact(text)
                    .map(Cow::Owned)
                    .unwrap_or(Cow::Borrowed(text))
            }
            McpContentDisplay::Summary(summary) => Cow::Borrowed(summary),
        }
    }
}
