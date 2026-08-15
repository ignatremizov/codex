use super::*;
use pretty_assertions::assert_eq;

#[test]
fn only_known_active_target_rejection_permits_automatic_queue_deferral() {
    let message = "queued input requires an idle target; agent already has an active turn";
    let mut expected = invalid_request(message);
    expected.data = Some(serde_json::json!({"reason": "targetActive"}));
    assert_eq!(
        agent_control_error(CodexErr::InvalidRequest(message.into())),
        expected
    );
    assert_eq!(
        agent_control_error(CodexErr::InvalidRequest("input outcome unknown".into())),
        invalid_request("input outcome unknown")
    );
}

#[test]
fn agent_control_reuses_current_inline_and_file_image_validation() {
    let file = V2UserInput::Image {
        image: codex_app_server_protocol::ImageReference::File {
            file_id: "file-1".into(),
        },
        detail: None,
    };
    let inline = |url: &str| V2UserInput::Image {
        image: codex_app_server_protocol::ImageReference::Inline { url: url.into() },
        detail: None,
    };
    assert_eq!(
        validate_user_input_image_urls(&[file, inline("data:image/png;base64,abc")]),
        Ok(())
    );
    assert_eq!(
        validate_user_input_image_urls(&[inline("https://example.test/image.png")]),
        Err(invalid_request(crate::image_url::REMOTE_IMAGE_URL_ERROR))
    );
}
