use super::ErrorNotification;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn retry_deadline_is_nullable_and_legacy_notifications_remain_readable() -> serde_json::Result<()> {
    let mut expected = json!({
        "error": {
            "message": "Model at capacity. Retrying... 1/5",
            "codexErrorInfo": "serverOverloaded",
            "additionalDetails": null,
            "misalignment": null
        },
        "willRetry": true,
        "threadId": "thread",
        "turnId": "turn"
    });
    let legacy: ErrorNotification = serde_json::from_value(expected.clone())?;
    expected["retryAt"] = json!(null);
    assert_eq!(serde_json::to_value(legacy)?, expected);
    expected["retryAt"] = json!(1_800_000_000);
    let timed: ErrorNotification = serde_json::from_value(expected.clone())?;
    assert_eq!(serde_json::to_value(timed)?, expected);
    Ok(())
}
