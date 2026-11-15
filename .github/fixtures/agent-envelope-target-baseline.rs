//! Remote-only serializer fixture over the pinned upstream production implementation.
//! The preparation receipt records the exact source, harness, and test-only overlay.

use super::agent_message_from_tool;
use super::message_tool::FollowupTaskArgs;
use super::message_tool::message_content;
use crate::agent::types::MessageDeliveryMode;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::multi_agents_spec::SpawnAgentToolOptions;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_followup_task_tool;
use crate::tools::handlers::multi_agents_spec::create_interrupt_agent_tool_v2;
use crate::tools::handlers::multi_agents_spec::create_list_agents_tool;
use crate::tools::handlers::multi_agents_spec::create_resume_agent_tool;
use crate::tools::handlers::multi_agents_spec::create_send_input_tool_v1;
use crate::tools::handlers::multi_agents_spec::create_send_message_tool;
use crate::tools::handlers::multi_agents_spec::create_spawn_agent_tool_v1;
use crate::tools::handlers::multi_agents_spec::create_spawn_agent_tool_v2;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v1;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use anyhow::Context;
use codex_protocol::AgentPath;
use codex_protocol::protocol::MultiAgentVersion;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

fn record(
    rows: &mut Vec<Value>,
    case: &str,
    representation: &str,
    value: Value,
    authored_utf8_bytes: usize,
    ciphertext_utf8_bytes: usize,
) -> anyhow::Result<()> {
    let serialized_utf8_bytes = serde_json::to_vec(&value)?.len();
    let framing = serialized_utf8_bytes
        .checked_sub(authored_utf8_bytes + ciphertext_utf8_bytes)
        .context("serialized fixture cannot be smaller than its complete payload")?;
    rows.push(json!({
        "case": case,
        "representation": representation,
        "serialized_utf8_bytes": serialized_utf8_bytes,
        "authored_utf8_bytes": authored_utf8_bytes,
        "ciphertext_utf8_bytes": ciphertext_utf8_bytes,
        "json_framing_and_escaping_utf8_bytes": framing,
        "value": value,
    }));
    Ok(())
}

#[test]
fn capture_pinned_target_agent_envelopes() -> anyhow::Result<()> {
    let mut rows = Vec::new();
    for (name, spec) in [
        (
            "upstream_v1.spawn",
            create_spawn_agent_tool_v1(SpawnAgentToolOptions::default()),
        ),
        ("upstream_v1.send_input", create_send_input_tool_v1()),
        ("upstream_v1.resume", create_resume_agent_tool()),
        (
            "upstream_v1.wait",
            create_wait_agent_tool_v1(WaitAgentTimeoutOptions::default()),
        ),
        (
            "v2.spawn",
            create_spawn_agent_tool_v2(
                SpawnAgentToolOptions {
                    multi_agent_version: MultiAgentVersion::V2,
                    ..Default::default()
                },
                /*description_override*/ None,
            ),
        ),
        ("v2.send_message", create_send_message_tool()),
        ("v2.followup_task", create_followup_task_tool()),
        (
            "v2.wait",
            create_wait_agent_tool_v2(WaitAgentTimeoutOptions::default()),
        ),
        ("v2.roster", create_list_agents_tool()),
        ("v2.interrupt", create_interrupt_agent_tool_v2()),
    ] {
        record(
            &mut rows,
            name,
            "tool_spec",
            serde_json::to_value(spec)?,
            /*authored_utf8_bytes*/ 0,
            /*ciphertext_utf8_bytes*/ 0,
        )?;
    }

    let authored = "Inspect every file; preserve café and 日本語.\n".repeat(/*n*/ 12);
    let ciphertext = "opaque-fixture-not-real-ciphertext".repeat(/*n*/ 8);
    for (source_name, source, input, authored_bytes, ciphertext_bytes) in [
        (
            "direct",
            ToolCallSource::Direct,
            ciphertext.clone(),
            0,
            ciphertext.len(),
        ),
        (
            "direct_plaintext",
            ToolCallSource::DirectPlaintextMessage,
            authored.clone(),
            authored.len(),
            0,
        ),
        (
            "code_mode",
            ToolCallSource::CodeMode {
                cell_id: "cell-1".to_string(),
                runtime_tool_call_id: "nested-1".to_string(),
            },
            authored.clone(),
            authored.len(),
            0,
        ),
    ] {
        for (operation, mode) in [
            ("send_message", MessageDeliveryMode::QueueOnly),
            ("followup_task", MessageDeliveryMode::TriggerTurn),
        ] {
            let message = message_content(input.clone())?;
            let communication = agent_message_from_tool(message, &source).into_communication(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").map_err(anyhow::Error::msg)?,
                mode,
            );
            assert_eq!(
                communication.trigger_turn,
                mode == MessageDeliveryMode::TriggerTurn
            );
            if ciphertext_bytes != 0 {
                assert_eq!(
                    communication.encrypted_content.as_deref(),
                    Some(ciphertext.as_str())
                );
            } else {
                assert!(communication.encrypted_content.is_none());
                assert!(communication.content.contains(&authored));
            }
            let value = serde_json::to_value(communication)?;
            let case = format!("v2.source.{source_name}.{operation}");
            record(
                &mut rows,
                &case,
                "inter_agent_communication",
                value.clone(),
                authored_bytes,
                ciphertext_bytes,
            )?;
            record(
                &mut rows,
                &format!("{case}.repeated_32"),
                "inter_agent_communication_array",
                Value::Array(vec![value; 32]),
                authored_bytes * 32,
                ciphertext_bytes * 32,
            )?;
            record(
                &mut rows,
                &case,
                "invocation_arguments",
                json!({"target": "/root/worker", "message": input}),
                authored_bytes,
                ciphertext_bytes,
            )?;
        }
    }

    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let result = FunctionToolOutput::from_text(String::new(), /*success*/ Some(true));
    record(
        &mut rows,
        "v2.message",
        "direct_response_item",
        serde_json::to_value(result.to_response_item("input-1", &payload))?,
        /*authored_utf8_bytes*/ 0,
        /*ciphertext_utf8_bytes*/ 0,
    )?;
    record(
        &mut rows,
        "v2.message",
        "code_mode_result",
        result.code_mode_result(&payload),
        /*authored_utf8_bytes*/ 0,
        /*ciphertext_utf8_bytes*/ 0,
    )?;
    for (name, args) in [
        (
            "v2.w",
            json!({"target": "/root/worker", "message": ciphertext, "w": "f"}),
        ),
        (
            "v2.batch",
            json!({"target": ["/root/worker", "/root/reviewer"], "message": ciphertext}),
        ),
        (
            "v2.audit",
            json!({"target": "/root/worker", "message": ciphertext, "task_message": authored}),
        ),
    ] {
        let error = serde_json::from_value::<FollowupTaskArgs>(args)
            .expect_err("pinned target does not expose these fork input fields");
        record(
            &mut rows,
            name,
            "argument_parse_error_text",
            json!(error.to_string()),
            /*authored_utf8_bytes*/ 0,
            /*ciphertext_utf8_bytes*/ 0,
        )?;
    }
    let report = json!({
        "format": 1,
        "source_sha": "fa266e73c414c1798f6ada3f85a151044e07f7c4",
        "harness_sha": std::env::var("GITHUB_SHA").ok(),
        "evidence": "Pinned upstream production factories and serializers under a recorded test-only overlay. No tool dispatch, encryption operation, or receiving-turn admission is inferred.",
        "tokenizer": null,
        "unmeasured": ["actual spawn/resume receipts", "commentary/final delivery", "batch partial/unknown admission", "q/z receiver behavior", "end-to-end configured namespaces and Code Mode dispatch"],
        "rows": rows,
    });
    let directory = std::env::var_os("CODEX_TEST_AGENT_ENVELOPE_DIR")
        .context("remote producer requires its evidence directory")?;
    std::fs::write(
        std::path::PathBuf::from(directory).join("pinned-target-agent-envelopes.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}
