use super::*;

#[test]
fn text_placeholder_wrapper_is_dropped_when_only_the_opener_fits() {
    let opener = "<image name=[Image #1] path=\"/tmp/current.png\">";
    let item = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: opener.to_string(),
            },
            ContentItem::InputText {
                text: "[Image #1]".to_string(),
            },
            ContentItem::InputText {
                text: "</image>".to_string(),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };

    let truncated = truncate_retained_message_to_token_budget(
        item,
        approx_token_count(opener).saturating_add(1),
    );

    assert!(matches!(truncated, RetainedMessageTruncation::Empty));
}
