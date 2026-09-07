use crate::tools::handlers::multi_agents_spec::MULTI_AGENT_V1_NAMESPACE;
use crate::tools::handlers::multi_agents_spec::MULTI_AGENT_V1_NAMESPACE_DESCRIPTION;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub(super) fn create_list_agents_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "path_prefix".to_string(),
            JsonSchema::string(Some(
                "Exact task path or subtree prefix. Relative paths resolve against your task path, \
                 or /root if unset. Omit to search the entire owned graph."
                    .to_string(),
            )),
        ),
        (
            "status".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("loaded"),
                    json!("running"),
                    json!("pending_init"),
                    json!("completed"),
                    json!("interrupted"),
                    json!("errored"),
                    json!("closed"),
                    json!("all"),
                ],
                Some(
                    "Defaults to loaded, including agents awaiting more input. Use closed or all \
                     to include closed owned members."
                        .to_string(),
                ),
            ),
        ),
        (
            "cursor".to_string(),
            JsonSchema::string(Some(
                "Next cursor from a previous page with the same filters.".to_string(),
            )),
        ),
        (
            "limit".to_string(),
            JsonSchema::integer(Some("Maximum number of entries in this page.".to_string())),
        ),
    ]);

    let tool = ResponsesApiTool {
        name: "list_agents".to_string(),
        description: "Read the owned graph's agent directory in stable ref order, with filters \
                      applied before pagination. Requires explicit user enablement. Discovery \
                      grants no messaging or spawning permission, does not resume agents, and \
                      creates no response subscriptions."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            /*required*/ None,
            /*additional_properties*/ Some(false.into()),
        ),
        output_schema: Some(json!({
            "type": "object",
            "properties": {
                "agents": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "agent_id": {"type": "string"},
                            "ref": {"type": "string"},
                            "nickname": {"type": ["string", "null"]},
                            "role": {"type": ["string", "null"]},
                            "task_path": {"type": ["string", "null"]},
                            "status": {
                                "type": "string",
                                "enum": [
                                    "pending_init", "running", "completed", "interrupted",
                                    "errored", "closed", "unloaded"
                                ]
                            }
                        },
                        "required": ["agent_id", "ref", "nickname", "role", "task_path", "status"],
                        "additionalProperties": false
                    }
                },
                "next_cursor": {"type": ["string", "null"]}
            },
            "required": ["agents", "next_cursor"],
            "additionalProperties": false
        })),
    };
    ToolSpec::Namespace(ResponsesApiNamespace {
        name: MULTI_AGENT_V1_NAMESPACE.to_string(),
        description: MULTI_AGENT_V1_NAMESPACE_DESCRIPTION.to_string(),
        tools: vec![ResponsesApiNamespaceTool::Function(tool)],
    })
}
