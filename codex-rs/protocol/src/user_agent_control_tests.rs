use super::UserAgentControlAction;
use super::UserAgentControlItem;
use crate::ThreadId;
use pretty_assertions::assert_eq;

#[test]
fn legacy_control_records_default_observer_fields_without_changing_other_data() {
    let mut expected = UserAgentControlItem::succeeded(UserAgentControlAction::Observe);
    expected.authored_selector = Some("Main".to_string());
    expected.target_thread_id = Some(ThreadId::new());
    let mut legacy = serde_json::to_value(&expected).unwrap();
    let fields = legacy.as_object_mut().unwrap();
    fields.remove("observerThreadId");
    fields.remove("authoredObserverSelector");
    assert_eq!(
        serde_json::from_value::<UserAgentControlItem>(legacy).unwrap(),
        expected
    );
}

#[test]
fn explicit_observer_identity_and_selector_round_trip_in_audit() {
    let mut expected = UserAgentControlItem::succeeded(UserAgentControlAction::Observe);
    expected.authored_selector = Some("Main".to_string());
    expected.target_thread_id = Some(ThreadId::new());
    expected.observer_thread_id = Some(ThreadId::new());
    expected.authored_observer_selector = Some("2".to_string());
    assert_eq!(
        serde_json::from_value::<UserAgentControlItem>(serde_json::to_value(&expected).unwrap())
            .unwrap(),
        expected
    );
}
