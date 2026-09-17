pub mod apply_patch;
pub mod exec_command;
pub mod process_manager;
pub(crate) mod tool_runner;
pub mod types;
pub mod write_stdin;

use std::sync::Arc;

use rmcp::model::JsonObject;
use rmcp::model::Tool;
use schemars::r#gen::SchemaSettings;

pub use apply_patch::handle_apply_patch;
pub use exec_command::handle_exec_command;
pub use process_manager::HarnessProcessManager;
pub(crate) use tool_runner::dispatch_harness_tool_call;
pub use types::ApplyPatchParams;
pub use types::ApplyPatchResponse;
pub use types::ExecCommandParams;
pub use types::ExecCommandResponse;
pub use types::WriteStdinParams;
pub use types::WriteStdinResponse;
pub use write_stdin::handle_write_stdin;

pub(crate) fn bounded_wait_ms(requested: Option<u64>, default_ms: u64, max_ms: u64) -> u64 {
    requested.unwrap_or(default_ms).min(max_ms)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

pub fn create_tool_for_exec_command() -> Tool {
    let input_schema = create_tool_schema(
        SchemaSettings::draft2019_09()
            .with(|s| {
                s.inline_subschemas = true;
                s.option_add_null_type = false;
            })
            .into_generator()
            .into_root_schema_for::<ExecCommandParams>(),
        "exec_command input schema should serialize",
        &[],
    );

    let output_schema = create_tool_schema(
        SchemaSettings::draft2019_09()
            .with(|s| {
                s.inline_subschemas = true;
                s.option_add_null_type = false;
            })
            .into_generator()
            .into_root_schema_for::<ExecCommandResponse>(),
        "exec_command output schema should serialize",
        &["output"],
    );

    Tool::new(
        "exec_command",
        "Execute a shell command on the host. Before substantial repo work, read applicable AGENTS.md files and referenced/applicable SKILL.md files. Omit 'sandbox' by default unless the user explicitly requests sandboxing. A still-running command yields a resumable session after 60 seconds by default; requested yieldTimeMs values above 100 seconds are clamped so the MCP call returns before upstream transport timeouts. Yielding does not stop the process: retain sessionId and continue with write_stdin instead of starting the command again.",
        input_schema,
    )
    .with_title("Execute Command")
    .with_raw_output_schema(output_schema)
}

pub fn create_tool_for_write_stdin() -> Tool {
    let input_schema = create_tool_schema(
        SchemaSettings::draft2019_09()
            .with(|s| {
                s.inline_subschemas = true;
                s.option_add_null_type = false;
            })
            .into_generator()
            .into_root_schema_for::<WriteStdinParams>(),
        "write_stdin input schema should serialize",
        &[],
    );

    let output_schema = create_tool_schema(
        SchemaSettings::draft2019_09()
            .with(|s| {
                s.inline_subschemas = true;
                s.option_add_null_type = false;
            })
            .into_generator()
            .into_root_schema_for::<WriteStdinResponse>(),
        "write_stdin output schema should serialize",
        &["output"],
    );

    Tool::new(
        "write_stdin",
        "Send input/signals or retrieve output deltas from a running process session. Any requested wait is capped at 120 seconds because the upstream MCP transport can time out longer calls; for longer jobs, poll again using the same sessionId. Reading output advances the session's unread-output offsets; do not poll merely to monitor a long job when the user's local observer can watch it non-consumingly.",
        input_schema,
    )
    .with_title("Write Stdin")
    .with_raw_output_schema(output_schema)
}

pub fn create_tool_for_apply_patch() -> Tool {
    let input_schema = create_tool_schema(
        SchemaSettings::draft2019_09()
            .with(|s| {
                s.inline_subschemas = true;
                s.option_add_null_type = false;
            })
            .into_generator()
            .into_root_schema_for::<ApplyPatchParams>(),
        "apply_patch input schema should serialize",
        &[],
    );

    let output_schema = create_tool_schema(
        SchemaSettings::draft2019_09()
            .with(|s| {
                s.inline_subschemas = true;
                s.option_add_null_type = false;
            })
            .into_generator()
            .into_root_schema_for::<ApplyPatchResponse>(),
        "apply_patch output schema should serialize",
        &[],
    );

    Tool::new(
        "apply_patch",
        "Apply or dry-run validate a unified diff patch against the workspace. Before patching, read applicable AGENTS.md files and referenced/applicable SKILL.md files.",
        input_schema,
    )
    .with_title("Apply Patch")
    .with_raw_output_schema(output_schema)
}

fn create_tool_schema(
    schema: schemars::schema::RootSchema,
    panic_message: &str,
    optional_required_fields: &[&str],
) -> Arc<JsonObject> {
    #[expect(clippy::expect_used)]
    let schema_value = serde_json::to_value(&schema).expect(panic_message);
    let mut schema_object = match schema_value {
        serde_json::Value::Object(object) => object,
        _ => panic!("tool schema should serialize to a JSON object"),
    };

    let mut tool_schema = JsonObject::new();
    for key in [
        "additionalProperties",
        "properties",
        "required",
        "type",
        "$defs",
        "definitions",
    ] {
        if let Some(value) = schema_object.remove(key) {
            tool_schema.insert(key.to_string(), value);
        }
    }

    if let Some(serde_json::Value::Array(required)) = tool_schema.get_mut("required") {
        required.retain(|value| {
            value
                .as_str()
                .is_none_or(|field| !optional_required_fields.contains(&field))
        });
    }

    Arc::new(tool_schema)
}
