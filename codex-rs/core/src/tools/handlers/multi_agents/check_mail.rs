//! Selection only: accepted direct results carry consumption to the history recorder.

use super::*;
use crate::agent::agent_resolver::resolve_controlled_v1_agent_target;
use crate::session::mailbox::MailboxConsumption;
use crate::tools::context::ToolCallSource;
use crate::tools::handlers::multi_agents_spec::MULTI_AGENT_V1_NAMESPACE_DESCRIPTION;
use codex_protocol::protocol::MultiAgentVersion;
use codex_thread_store::MailboxSelection;
use codex_thread_store::MailboxSender;
use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use futures::future::BoxFuture;
use std::collections::BTreeMap;

pub(crate) struct Handler;

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, "check_mail")
    }

    fn exposure(&self) -> crate::tools::registry::ToolExposure {
        crate::tools::registry::ToolExposure::DirectModelOnly
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: MULTI_AGENT_V1_NAMESPACE.to_string(),
            description: MULTI_AGENT_V1_NAMESPACE_DESCRIPTION.to_string(),
            tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                name: "check_mail".to_string(),
                description: "Deliver and consume pending mailbox messages as structured input after this tool result. \
                    Omit from to consume all pending mail, or select one sender by agent ref, nickname, task path, \
                    full UUID, or user. The selected batch is fixed; later arrivals and unselected senders remain pending. \
                    This does not consume queued prompts or start another turn. Output contains metadata, not message bodies. \
                    Direct calls only; unavailable inside code-mode or nested tool execution.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: JsonSchema::object(
                    BTreeMap::from([("from".to_string(), JsonSchema::string(Some(
                        "Canonicalizable sender selector, or user; omission selects all pending mail.".to_string(),
                    )))]),
                    /*required*/ None,
                    /*additional_properties*/ Some(false.into()),
                ),
                output_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "status": {"type": "string", "enum": ["delivery_requested"]},
                        "from": {"type": ["string", "null"]}
                    },
                    "required": ["status", "from"],
                    "additionalProperties": false
                })),
            })],
        })
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        multi_agent_tool_search_info(
            "check_mail consume deliver pending mailbox mail sender user",
            self.spec(),
        )
    }

    fn handle<'a>(&'a self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async {
            Err(FunctionCallError::RespondToModel(
                "check_mail requires the direct ordered tool-result recorder; nested execution is unsupported".to_string(),
            ))
        })
    }
}

impl CoreToolRuntime for Handler {
    fn handle_with_mailbox_operation(
        &self,
        invocation: ToolInvocation,
    ) -> BoxFuture<'_, Result<(Box<dyn ToolOutput>, Option<MailboxConsumption>), FunctionCallError>>
    {
        Box::pin(async move {
            if invocation.source != ToolCallSource::Direct {
                return Err(FunctionCallError::RespondToModel(
                    "check_mail is disabled in nested/code-mode execution until the outer result supports ordered delivery".to_string(),
                ));
            }
            if invocation.turn.multi_agent_version != MultiAgentVersion::V1 {
                return Err(FunctionCallError::RespondToModel(
                    "check_mail is only available for the V1 receiver-selected mailbox".to_string(),
                ));
            }
            let arguments = function_arguments(invocation.payload.clone())?;
            let args: CheckMailArgs = parse_arguments(&arguments)?;
            let (selection, sender) = match args.from.as_deref() {
                None => (MailboxSelection::All, None),
                Some("user") => (
                    MailboxSelection::Senders(vec![MailboxSender::User]),
                    Some("user".to_string()),
                ),
                Some(selector) => {
                    // Canonical UUID selection is independent of lifecycle
                    // ownership. Other selectors use the durable V1 namespace,
                    // including closed aliases, without loading the sender.
                    let canonical = selector.strip_prefix("id:").unwrap_or(selector);
                    let sender = match ThreadId::from_string(canonical) {
                        Ok(sender) => sender,
                        Err(_) => {
                            resolve_controlled_v1_agent_target(&invocation.session, selector)
                                .await?
                        }
                    };
                    (
                        MailboxSelection::Senders(vec![MailboxSender::Agent(sender)]),
                        Some(sender.to_string()),
                    )
                }
            };
            let output = boxed_tool_output(JsonToolOutput::new(serde_json::json!({
                "status": "delivery_requested",
                "from": sender,
            })));
            Ok((
                output,
                Some(MailboxConsumption {
                    tool_call_id: invocation.call_id,
                    selection,
                }),
            ))
        })
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckMailArgs {
    from: Option<String>,
}

#[cfg(test)]
#[path = "check_mail_tests.rs"]
mod tests;
