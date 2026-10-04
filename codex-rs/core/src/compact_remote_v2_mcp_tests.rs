use super::*;
use crate::context::ContextualUserFragment;
use codex_protocol::ResponseItemId;
use pretty_assertions::assert_eq;
use test_case::test_case;

fn message(role: &str, text: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    })
}

fn explicit_inventory(id: &str, server_name: &str, inventory: &str) -> ResponseItemEnvelope {
    let mut item: ResponseItem = ContextualUserFragment::into(McpServerUseInstructions::new(
        server_name.to_string(),
        inventory.to_string(),
    ));
    item.set_id(Some(ResponseItemId::with_suffix("msg", id)));
    ResponseItemEnvelope {
        item,
        metadata: Some(CodexHarnessMetadata {
            client_authored: true,
            history_truncation_token_limit: Some(1),
            user_input_order: Some(7),
            ..Default::default()
        }),
    }
}

#[test_case(false; "generic developer retention disabled")]
#[test_case(true; "generic developer retention enabled")]
fn repeated_remote_compaction_preserves_each_explicit_envelope_once(
    retain_client_developer_messages: bool,
) {
    let huge_inventory = serde_json::json!([{
        "description": "old ".repeat(RETAINED_MESSAGE_TOKEN_BUDGET + 1),
    }])
    .to_string();
    let mut first = explicit_inventory("first", "linear", &huge_inventory);
    first
        .metadata
        .as_mut()
        .expect("fixture metadata")
        .client_authored = false;
    let second = explicit_inventory("second", "github", r#"["issues"]"#);
    // The same accepted text in distinct envelopes is not a license to coalesce them.
    let third = explicit_inventory("third", "github", r#"["issues"]"#);
    let fourth = explicit_inventory("fourth", "linear", r#"["updated"]"#);
    let mut client = message("developer", "ordinary client instructions");
    client.metadata = Some(CodexHarnessMetadata {
        client_authored: true,
        ..Default::default()
    });
    let mut history = vec![
        first.clone(),
        message("developer", "ordinary harness context"),
        client.clone(),
        second.clone(),
        message("user", "retained prompt"),
        third.clone(),
        fourth.clone(),
    ];
    let checkpoint = ResponseItem::Compaction {
        id: None,
        encrypted_content: "opaque checkpoint".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let mut expected = vec![
        first,
        second,
        message("user", "retained prompt"),
        third,
        fourth,
    ];
    if retain_client_developer_messages {
        expected.insert(1, client);
    }
    expected.push(ResponseItemEnvelope::new(checkpoint.clone()));
    for _ in 0..2 {
        let (items, metadata) = history
            .into_iter()
            .map(|envelope| (envelope.item, envelope.metadata))
            .unzip();
        let (compacted, retained_images) = build_v2_compacted_history(
            items,
            metadata,
            checkpoint.clone(),
            retain_client_developer_messages,
            RetainedImageBudget::Enabled,
        );
        assert_eq!(compacted, expected);
        assert_eq!(retained_images, 0);
        history = compacted;
    }
}

#[test_case(0; "zero generic budget")]
#[test_case(1; "exhausted generic budget")]
fn explicit_inventory_bypasses_budget_without_losing_attached_notice(max_tokens: usize) {
    let first = explicit_inventory("first", "linear", r#"["first"]"#);
    let notice = message(
        "developer",
        "<image_resize_notice>generated</image_resize_notice>",
    );
    let second = explicit_inventory("second", "linear", r#"["second"]"#);
    let newest = message("user", "new");
    let history = vec![
        first.clone(),
        notice.clone(),
        second.clone(),
        newest.clone(),
    ];
    let mut expected = vec![first, notice, second];
    if max_tokens > 0 {
        expected.push(newest);
    }
    assert_eq!(
        truncate_retained_messages(history, max_tokens, RetainedImageBudget::Enabled),
        expected,
    );
}
