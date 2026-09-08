use super::*;
use pretty_assertions::assert_eq;

#[test]
fn inventory_identity_is_distinct_and_cannot_be_used_by_provider_output() {
    let notification = uuid::Uuid::now_v7().to_string();
    let inventory = mailbox_inventory_response_item_id(&notification).unwrap();
    let delivery = mailbox_delivery_response_item_id(&notification).unwrap();
    assert!(is_mailbox_inventory_response_item_id(inventory.as_str()));
    assert!(!is_mailbox_delivery_response_item_id(inventory.as_str()));
    assert!(!is_mailbox_inventory_response_item_id(delivery.as_str()));
    assert_eq!(
        crate::sub_agent_completion::ordinary_agent_message_response_item_id(inventory.as_str()),
        format!("agent_{inventory}")
    );
    assert_eq!(mailbox_inventory_response_item_id("not-a-uuid"), None);
    assert!(!is_mailbox_inventory_response_item_id(
        "msg_mailbox_inventory_invalid"
    ));
}

#[test]
fn ordinary_provider_messages_cannot_use_trusted_mailbox_identity() {
    let id = mailbox_delivery_response_item_id(&uuid::Uuid::now_v7().to_string()).unwrap();
    assert_eq!(
        crate::sub_agent_completion::ordinary_agent_message_response_item_id(id.as_str()),
        format!("agent_{id}")
    );
    for ordinary in ["provider-id", "msg_mailbox_invalid", "msg_mailboxish_123"] {
        assert_eq!(
            crate::sub_agent_completion::ordinary_agent_message_response_item_id(ordinary),
            ordinary
        );
    }
}

#[test]
fn delivery_identity_requires_canonical_uuid_v7() {
    let delivery_id = uuid::Uuid::now_v7().to_string();
    let id = mailbox_delivery_response_item_id(&delivery_id).unwrap();
    assert_eq!(id.to_string(), format!("msg_mailbox_{delivery_id}"));
    assert!(is_mailbox_delivery_response_item_id(id.as_str()));
    for invalid in [
        delivery_id.replace('-', ""),
        "00000000-0000-4000-8000-000000000000".to_string(),
        "not-a-uuid".to_string(),
        format!("{delivery_id}_extra"),
    ] {
        assert_eq!(mailbox_delivery_response_item_id(&invalid), None);
        assert!(!is_mailbox_delivery_response_item_id(&format!(
            "msg_mailbox_{invalid}"
        )));
    }
    for invalid in [
        format!("msg_{delivery_id}"),
        format!("agent_msg_mailbox_{delivery_id}"),
        format!("msg_mailbox_{delivery_id}_extra"),
    ] {
        assert!(!is_mailbox_delivery_response_item_id(&invalid));
    }
}
