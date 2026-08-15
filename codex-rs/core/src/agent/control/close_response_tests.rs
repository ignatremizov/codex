use super::*;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use pretty_assertions::assert_eq;

fn wait_call(call_id: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        name: "wait_agent".to_string(),
        namespace: Some("multi_agent_v1".to_string()),
        arguments: r#"{"targets":["2"]}"#.to_string(),
        encrypted_function_args: None,
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn tool_output(call_id: &str, text: String) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(call_id.to_string()),
        name: None,
        namespace: None,
        output: FunctionCallOutputPayload::from_text(text),
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn completion_dedupe_accepts_canonical_envelope_or_matching_wait_output() {
    let references = vec![
        "019faa07-aa3d-78d3-9eca-66cd8626adad".to_string(),
        "2".to_string(),
        "Pascal".to_string(),
        "/root/worker".to_string(),
        "id:019faa07-aa3d-78d3-9eca-66cd8626adad".to_string(),
        "ref:2".to_string(),
        "nick:Pascal".to_string(),
    ];
    let payload = "result with a newline\nand \"quotes\"";
    let expected_message = "canonical completion envelope";
    let canonical = ResponseItem::AgentMessage {
        id: None,
        author: "/root/worker".to_string(),
        recipient: "/root".to_string(),
        content: vec![AgentMessageInputContent::InputText {
            text: expected_message.to_string(),
        }],
        internal_chat_message_metadata_passthrough: None,
    };
    let wait_call = wait_call("wait-call");
    let wait_call_ids = HashSet::from([wait_agent_call_id(&wait_call).expect("wait call ID")]);
    let wait_output = tool_output(
        "wait-call",
        serde_json::json!({
            "status": {
                "2": {
                    "completed": payload,
                }
            }
        })
        .to_string(),
    );

    assert!(response_item_contains_completion(
        &canonical,
        &wait_call_ids,
        "/root",
        &references,
        payload,
        expected_message,
    ));
    assert!(response_item_contains_completion(
        &wait_output,
        &wait_call_ids,
        "/root",
        &references,
        payload,
        expected_message,
    ));
    for target in [
        "id:019faa07-aa3d-78d3-9eca-66cd8626adad",
        "ref:2",
        "nick:Pascal",
    ] {
        let prefixed_output = tool_output(
            "wait-call",
            serde_json::json!({
                "status": {
                    (target): {
                        "completed": payload,
                    }
                }
            })
            .to_string(),
        );
        assert!(response_item_contains_completion(
            &prefixed_output,
            &wait_call_ids,
            "/root",
            &references,
            payload,
            expected_message,
        ));
    }
}

#[test]
fn completion_dedupe_rejects_quoted_envelopes_json_prose_and_unrelated_outputs() {
    let references = vec!["2".to_string(), "/root/worker".to_string()];
    let payload = "done";
    let expected_message = "canonical completion envelope";
    let wait_call = wait_call("wait-call");
    let wait_call_ids = HashSet::from([wait_agent_call_id(&wait_call).expect("wait call ID")]);
    let json_status = serde_json::json!({
        "status": {
            "2": {
                "completed": payload,
            }
        }
    })
    .to_string();
    let quoted_envelope = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: expected_message.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let json_prose = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: json_status.clone(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let unrelated_output = tool_output("different-call", json_status);
    let wrong_recipient = ResponseItem::AgentMessage {
        id: None,
        author: "/root/worker".to_string(),
        recipient: "/root/other".to_string(),
        content: vec![AgentMessageInputContent::InputText {
            text: expected_message.to_string(),
        }],
        internal_chat_message_metadata_passthrough: None,
    };

    for item in [
        quoted_envelope,
        json_prose,
        unrelated_output,
        wrong_recipient,
    ] {
        assert_eq!(
            response_item_contains_completion(
                &item,
                &wait_call_ids,
                "/root",
                &references,
                payload,
                expected_message,
            ),
            false
        );
    }
}
