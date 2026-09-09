use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

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
