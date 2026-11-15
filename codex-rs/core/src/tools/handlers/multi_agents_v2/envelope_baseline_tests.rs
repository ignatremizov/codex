//! Remote producer for exact serialized contract fixtures before V2 input migration.
//! These are factory/serializer measurements, not delivery, latency, or token-count evidence.

use super::message_tool::FollowupTaskArgs;
use super::message_tool::prepare_agent_message;
use crate::agent::input_policy::SendInputMode;
use crate::agent::types::MessageDeliveryMode;
use crate::config::MultiAgentMessageDelivery;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::multi_agents_input::SendInputAdmissionStatus;
use crate::tools::handlers::multi_agents_input::SendInputResult;
use crate::tools::handlers::multi_agents_resume_spec::create_resume_agent_tool_v2;
use crate::tools::handlers::multi_agents_spec::SpawnAgentToolOptions;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_followup_task_tool;
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
fn capture_pre_migration_agent_envelopes() -> anyhow::Result<()> {
    let mut rows = Vec::new();
    for (name, spec) in [
        (
            "v1.spawn",
            create_spawn_agent_tool_v1(SpawnAgentToolOptions::default()),
        ),
        ("v1.send_input", create_send_input_tool_v1()),
        ("v1.resume", create_resume_agent_tool()),
        (
            "v1.wait",
            create_wait_agent_tool_v1(WaitAgentTimeoutOptions::default()),
        ),
        (
            "v2.spawn",
            create_spawn_agent_tool_v2(
                SpawnAgentToolOptions::default(),
                /*description_override*/ None,
            ),
        ),
        (
            "v2.send_message",
            create_send_message_tool(MultiAgentMessageDelivery::EncryptedWithAudit),
        ),
        (
            "v2.followup_task",
            create_followup_task_tool(MultiAgentMessageDelivery::EncryptedWithAudit),
        ),
        ("v2.resume", create_resume_agent_tool_v2()),
        (
            "v2.wait",
            create_wait_agent_tool_v2(WaitAgentTimeoutOptions::default()),
        ),
        ("v2.roster", create_list_agents_tool()),
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
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    for (name, status) in [
        ("v1.ordinary", SendInputAdmissionStatus::Submitted),
        ("v1.q", SendInputAdmissionStatus::Queued),
        ("v1.z", SendInputAdmissionStatus::MailboxAccepted),
    ] {
        let result = SendInputResult {
            submission_id: "01953800-0000-7000-8000-000000000001".to_string(),
            status,
            hint: None,
        };
        record(
            &mut rows,
            name,
            "canonical_log",
            serde_json::from_str(&result.log_output())?,
            /*authored_utf8_bytes*/ 0,
            /*ciphertext_utf8_bytes*/ 0,
        )?;
        record(
            &mut rows,
            name,
            "direct_response_item",
            serde_json::to_value(result.to_response_item("input-1", &payload))?,
            /*authored_utf8_bytes*/ 0,
            /*ciphertext_utf8_bytes*/ 0,
        )?;
        record(
            &mut rows,
            name,
            "code_mode_result",
            result.code_mode_result(&payload),
            /*authored_utf8_bytes*/ 0,
            /*ciphertext_utf8_bytes*/ 0,
        )?;
    }
    let current_v2_result =
        FunctionToolOutput::from_text(String::new(), /*success*/ Some(true));
    record(
        &mut rows,
        "v2.message",
        "direct_response_item",
        serde_json::to_value(current_v2_result.to_response_item("input-1", &payload))?,
        /*authored_utf8_bytes*/ 0,
        /*ciphertext_utf8_bytes*/ 0,
    )?;
    record(
        &mut rows,
        "v2.message",
        "code_mode_result",
        current_v2_result.code_mode_result(&payload),
        /*authored_utf8_bytes*/ 0,
        /*ciphertext_utf8_bytes*/ 0,
    )?;

    for (name, delivery) in [
        ("v2.plaintext", MultiAgentMessageDelivery::Plaintext),
        ("v2.encrypted", MultiAgentMessageDelivery::Encrypted),
        (
            "v2.encrypted_with_audit",
            MultiAgentMessageDelivery::EncryptedWithAudit,
        ),
    ] {
        for (operation, mode) in [
            ("send_message", MessageDeliveryMode::QueueOnly),
            ("followup_task", MessageDeliveryMode::TriggerTurn),
        ] {
            let message = if delivery == MultiAgentMessageDelivery::Plaintext {
                authored.clone()
            } else {
                ciphertext.clone()
            };
            let audit = (delivery == MultiAgentMessageDelivery::EncryptedWithAudit)
                .then(|| authored.clone());
            let message = prepare_agent_message(message, audit, delivery, &ToolCallSource::Direct)?;
            let communication = message.into_communication(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").map_err(anyhow::Error::msg)?,
                mode,
            );
            let authored_bytes = if delivery == MultiAgentMessageDelivery::Encrypted {
                0
            } else {
                authored.len()
            };
            let ciphertext_bytes = if delivery == MultiAgentMessageDelivery::Plaintext {
                0
            } else {
                ciphertext.len()
            };
            if delivery == MultiAgentMessageDelivery::EncryptedWithAudit {
                assert_eq!(communication.content, authored);
                assert_eq!(
                    communication.encrypted_content.as_deref(),
                    Some(ciphertext.as_str())
                );
            }
            let value = serde_json::to_value(communication)?;
            record(
                &mut rows,
                &format!("{name}.{operation}"),
                "inter_agent_communication",
                value.clone(),
                authored_bytes,
                ciphertext_bytes,
            )?;
            record(
                &mut rows,
                &format!("{name}.{operation}.repeated_32"),
                "inter_agent_communication_array",
                Value::Array(vec![value; 32]),
                authored_bytes * 32,
                ciphertext_bytes * 32,
            )?;
        }
    }

    for flags in ["", "c", "f", "x", "fx", "q", "z", "zf"] {
        // Omitted w resolves per receiver; an explicit empty w is invalid.
        let args = if flags.is_empty() {
            assert!(serde_json::from_value::<SendInputMode>(json!(flags)).is_err());
            json!({"target": "ref:1", "message": authored})
        } else {
            let _: SendInputMode = serde_json::from_value(json!(flags))?;
            json!({"target": "ref:1", "message": authored, "w": flags})
        };
        let name = if flags.is_empty() {
            "v1.w.omitted".to_string()
        } else {
            format!("v1.w.{flags}")
        };
        record(
            &mut rows,
            &name,
            "invocation_arguments",
            args,
            authored.len(),
            /*ciphertext_utf8_bytes*/ 0,
        )?;
        let v2_args = if flags.is_empty() {
            json!({"target": "ref:1", "message": ciphertext, "task_message": authored})
        } else {
            json!({"target": "ref:1", "message": ciphertext, "task_message": authored, "w": flags})
        };
        assert_eq!(
            serde_json::from_value::<FollowupTaskArgs>(v2_args).is_ok(),
            flags.is_empty(),
            "current V2 allows omitted w but cannot express explicit w policies"
        );
    }
    record(
        &mut rows,
        "v1.batch",
        "invocation_arguments",
        json!({"target": ["ref:1", "task:/root/worker", "ref:2"], "message": authored}),
        authored.len(),
        /*ciphertext_utf8_bytes*/ 0,
    )?;

    let report = json!({
        "format": 1,
        "source_sha": std::env::var("GITHUB_SHA").ok(),
        "evidence": "Production factory/serializer output for fixed fixtures; no tool dispatch or recipient execution is inferred.",
        "tokenizer": null,
        "unmeasured": ["pinned target-upstream baseline", "actual spawn/resume receipts", "commentary/final delivery", "batch partial/unknown admission", "q/z receiver behavior", "end-to-end configured namespaces and Code Mode dispatch"],
        "rows": rows,
    });
    if let Some(directory) = std::env::var_os("CODEX_TEST_AGENT_ENVELOPE_DIR") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory)?;
        std::fs::write(
            directory.join("pre-migration-agent-envelopes.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
    }
    Ok(())
}
