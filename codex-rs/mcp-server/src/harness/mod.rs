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

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

pub fn create_tool_for_exec_command() -> Tool {
    let schema = SchemaSettings::draft2019_09()
        .with(|s| {
            s.inline_subschemas = true;
            s.option_add_null_type = false;
        })
        .into_generator()
        .into_root_schema_for::<ExecCommandParams>();

    let input_schema = create_tool_input_schema(schema, "exec_command schema should serialize");

    Tool::new(
        "exec_command",
        "Execute a shell command with sandboxing, Starlark policy enforcement, and yield control.",
        input_schema,
    )
    .with_title("Execute Command")
}

pub fn create_tool_for_write_stdin() -> Tool {
    let schema = SchemaSettings::draft2019_09()
        .with(|s| {
            s.inline_subschemas = true;
            s.option_add_null_type = false;
        })
        .into_generator()
        .into_root_schema_for::<WriteStdinParams>();

    let input_schema = create_tool_input_schema(schema, "write_stdin schema should serialize");

    Tool::new(
        "write_stdin",
        "Send standard input characters, signals, or poll output deltas from a running process session.",
        input_schema,
    )
    .with_title("Write Stdin")
}

pub fn create_tool_for_apply_patch() -> Tool {
    let schema = SchemaSettings::draft2019_09()
        .with(|s| {
            s.inline_subschemas = true;
            s.option_add_null_type = false;
        })
        .into_generator()
        .into_root_schema_for::<ApplyPatchParams>();

    let input_schema = create_tool_input_schema(schema, "apply_patch schema should serialize");

    Tool::new(
        "apply_patch",
        "Apply or dry-run validate a unified diff patch against the workspace.",
        input_schema,
    )
    .with_title("Apply Patch")
}

fn create_tool_input_schema(
    schema: schemars::schema::RootSchema,
    panic_message: &str,
) -> Arc<JsonObject> {
    #[expect(clippy::expect_used)]
    let schema_value = serde_json::to_value(&schema).expect(panic_message);
    let mut schema_object = match schema_value {
        serde_json::Value::Object(object) => object,
        _ => panic!("tool schema should serialize to a JSON object"),
    };

    let mut input_schema = JsonObject::new();
    for key in [
        "additionalProperties",
        "properties",
        "required",
        "type",
        "$defs",
        "definitions",
    ] {
        if let Some(value) = schema_object.remove(key) {
            input_schema.insert(key.to_string(), value);
        }
    }

    Arc::new(input_schema)
}
