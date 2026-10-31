//! Shared explicit resume choices and the V2 resume tool's schema.

use super::multi_agents_spec::agent_status_output_schema;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub(super) fn resume_model_properties() -> BTreeMap<String, JsonSchema> {
    BTreeMap::from([
        (
            "model".to_string(),
            JsonSchema::string(Some(
                "Optional model override for the resumed agent's future turns. Omit to keep its saved model. Use only when explicitly requested by the user or applicable AGENTS.md/skill instructions."
                    .to_string(),
            )),
        ),
        (
            "reasoning_effort".to_string(),
            JsonSchema::string(Some(
                "Optional reasoning effort override for the resumed agent's future turns. Omit to keep its saved effort. The resulting model/effort pair must be supported."
                    .to_string(),
            )),
        ),
    ])
}

pub(super) fn create_resume_agent_tool_v2() -> ToolSpec {
    let mut properties = resume_model_properties();
    properties.insert(
        "target".to_string(),
        JsonSchema::string(Some(
            "Owned agent id or canonical task name to resume (from spawn_agent).".to_string(),
        )),
    );
    ToolSpec::Function(ResponsesApiTool {
        name: "resume_agent".to_string(),
        description: "Restore an owned non-root agent without sending a message. Optional model and reasoning_effort update its future turns, including when it is already loaded, without interrupting current work. Omitted fields preserve the target's saved settings. Use explicit overrides only when requested by the user or applicable AGENTS.md/skill instructions. The recorded parent must be loaded. Cannot target the current thread or adopt another root's agent."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(properties, Some(vec!["target".to_string()]), Some(false.into())),
        output_schema: Some(json!({
            "type": "object",
            "properties": {
                "task_name": {"type": "string"},
                "status": agent_status_output_schema()
            },
            "required": ["task_name", "status"],
            "additionalProperties": false
        }).into()),
    })
}
