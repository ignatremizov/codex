use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn targets_preserve_string_and_array_call_shapes() {
    for (value, batch) in [
        (json!("43"), false),
        (json!(["43"]), true),
        (json!(["43", "44"]), true),
    ] {
        let args: SendInputArgs =
            serde_json::from_value(json!({"target": value, "message": "hello"})).unwrap();
        assert_eq!(matches!(args.target, SendInputTarget::Batch(_)), batch);
    }
    for value in [json!(43), json!([43]), json!(null), json!({})] {
        assert!(serde_json::from_value::<SendInputArgs>(json!({"target": value})).is_err());
    }
}

#[test]
fn shared_batch_flags_keep_normalized_single_recipient_semantics() {
    for (flags, expected) in [
        ("cf", "cf"),
        ("mmq", "mq"),
        ("ffx", "f"),
        ("fx", ""),
        ("xxf", "x"),
        ("z", "z"),
        ("fz", "zf"),
        ("zffx", "zf"),
        ("zfx", "z"),
        ("zfxx", "z"),
        ("zz", "z"),
    ] {
        let mode: SendInputMode = serde_json::from_value(json!(flags)).unwrap();
        assert_eq!(mode.normalized_flags(), expected);
    }
    for flags in ["", "zc", "zm", "zq", "unknown"] {
        assert!(serde_json::from_value::<SendInputMode>(json!(flags)).is_err());
    }
}

#[test]
fn admission_projection_keeps_internal_identifiers_out_of_model_results() {
    for (status, expected) in [
        (SendInputAdmissionStatus::Submitted, "submitted"),
        (SendInputAdmissionStatus::Queued, "queued"),
        (SendInputAdmissionStatus::MailboxAccepted, "mailboxAccepted"),
    ] {
        let result = SendInputResult {
            submission_id: "internal-admission-id".to_string(),
            status,
            hint: None,
        };
        let payload = ToolPayload::Function {
            arguments: "{}".to_string(),
        };
        assert_eq!(
            result.code_mode_result(&payload),
            json!({"status": expected})
        );
        assert_eq!(
            serde_json::from_str::<JsonValue>(&result.log_output()).expect("internal log"),
            json!({"submission_id": "internal-admission-id", "status": expected})
        );
        let ResponseInputItem::FunctionCallOutput { output, .. } =
            result.to_response_item("send-call", &payload)
        else {
            panic!("expected function output");
        };
        let codex_protocol::models::FunctionCallOutputBody::Text(text) = output.body else {
            panic!("expected JSON text");
        };
        assert_eq!(
            serde_json::from_str::<JsonValue>(&text).expect("model result"),
            json!({"status": expected})
        );
    }
}

#[test]
fn unloaded_mailbox_hint_is_model_visible_without_exposing_internal_identifiers() {
    let result = SendInputResult {
        submission_id: "internal-admission-id".to_string(),
        status: SendInputAdmissionStatus::MailboxAccepted,
        hint: Some("Mail saved; receiver not loaded. Use resume_agent first.".to_string()),
    };
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let expected = json!({
        "status": "mailboxAccepted",
        "hint": "Mail saved; receiver not loaded. Use resume_agent first."
    });
    assert_eq!(result.code_mode_result(&payload), expected);
    assert_eq!(
        serde_json::from_str::<JsonValue>(&result.log_output()).expect("internal log"),
        json!({
            "submission_id": "internal-admission-id",
            "status": "mailboxAccepted",
            "hint": "Mail saved; receiver not loaded. Use resume_agent first."
        })
    );
    let ResponseInputItem::FunctionCallOutput { output, .. } =
        result.to_response_item("send-call", &payload)
    else {
        panic!("expected function output");
    };
    let codex_protocol::models::FunctionCallOutputBody::Text(text) = output.body else {
        panic!("expected JSON text");
    };
    assert_eq!(
        serde_json::from_str::<JsonValue>(&text).expect("model result"),
        expected
    );
}
