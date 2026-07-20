use super::*;
use crate::AgentPath;
use crate::protocol::SubAgentActivityKind;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn activity_prompt_survives_legacy_conversion() -> serde_json::Result<()> {
    let child = ThreadId::new();
    for prompt in [
        None,
        Some("Review the patch.\n  Preserve this detail.".to_string()),
    ] {
        let item = SubAgentActivityItem {
            id: "activity-1".to_string(),
            kind: SubAgentActivityKind::Started,
            agent_thread_id: child,
            agent_path: AgentPath::root(),
            prompt: prompt.clone(),
        };
        let expected = SubAgentActivityEvent {
            event_id: item.id.clone(),
            occurred_at_ms: 42,
            agent_thread_id: child,
            agent_path: AgentPath::root(),
            kind: SubAgentActivityKind::Started,
            prompt,
        };
        let EventMsg::SubAgentActivity(actual) = item.as_legacy_event(/*occurred_at_ms*/ 42) else {
            panic!("activity conversion must produce a sub-agent activity event");
        };
        assert_eq!(actual, expected);
        assert_eq!(
            serde_json::from_value::<SubAgentActivityItem>(serde_json::to_value(&item)?)?,
            item,
        );
        assert_eq!(
            serde_json::from_value::<SubAgentActivityEvent>(serde_json::to_value(&expected)?)?,
            expected,
        );
    }
    Ok(())
}

#[test]
fn historical_activity_without_prompt_remains_readable() -> serde_json::Result<()> {
    let child = ThreadId::new();
    let item: SubAgentActivityItem = serde_json::from_value(json!({
        "id": "old-activity",
        "kind": "started",
        "agent_thread_id": child,
        "agent_path": "/root",
    }))?;
    assert_eq!(
        item,
        SubAgentActivityItem {
            id: "old-activity".to_string(),
            kind: SubAgentActivityKind::Started,
            agent_thread_id: child,
            agent_path: AgentPath::root(),
            prompt: None,
        },
    );
    let event: SubAgentActivityEvent = serde_json::from_value(json!({
        "event_id": "old-activity",
        "kind": "started",
        "agent_thread_id": child,
        "agent_path": "/root",
    }))?;
    assert_eq!(
        event,
        SubAgentActivityEvent {
            event_id: "old-activity".to_string(),
            occurred_at_ms: 0,
            agent_thread_id: child,
            agent_path: AgentPath::root(),
            kind: SubAgentActivityKind::Started,
            prompt: None,
        },
    );
    Ok(())
}
