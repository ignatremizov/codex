use super::*;
use codex_protocol::ThreadId;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HasLegacyEvent;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::TurnStartedEvent;
use pretty_assertions::assert_eq;
use std::collections::HashMap;

#[test]
fn legacy_mirrors_preserve_live_policy_but_do_not_override_a_new_canonical_item() {
    for tool in [
        CollabAgentTool::SpawnAgent,
        CollabAgentTool::SendInput,
        CollabAgentTool::ResumeAgent,
    ] {
        let parent = ThreadId::new();
        let child = ThreadId::new();
        let mut item = CollabAgentToolCallItem {
            mailbox_input: None,
            target_messages: Some(true),
            queue_input: Some(false),
            id: "collaboration".to_string(),
            tool,
            status: CollabAgentToolCallStatus::InProgress,
            observe_commentary: Some(true),
            wake_on_completion: Some(false),
            deadline_at_ms: None,
            sender_thread_id: parent,
            receiver_thread_ids: vec![child],
            receiver_agents: Vec::new(),
            prompt: (!matches!(tool, CollabAgentTool::ResumeAgent)).then(|| "task".to_string()),
            model: matches!(tool, CollabAgentTool::SpawnAgent).then(|| "test-model".to_string()),
            reasoning_effort: matches!(tool, CollabAgentTool::SpawnAgent)
                .then_some(ReasoningEffort::High),
            agents_states: HashMap::from([(child, AgentStatus::Running)]),
            completion_presentation_agent_ids: None,
        };
        let mut builder = ThreadHistoryBuilder::new();
        builder.handle_event(&EventMsg::TurnStarted(TurnStartedEvent {
            agent_queue: None,
            turn_id: "parent-turn".to_string(),
            root_turn_id: None,
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        }));
        let started = EventMsg::ItemStarted(ItemStartedEvent {
            thread_id: parent,
            turn_id: "parent-turn".to_string(),
            item: TurnItem::CollabAgentToolCall(item.clone()),
            started_at_ms: 1,
        });
        builder.handle_event(&started);
        for mirror in started.as_legacy_events(/*show_raw_agent_reasoning*/ false) {
            builder.handle_event(&mirror);
        }
        item.status = CollabAgentToolCallStatus::Completed;
        item.agents_states =
            HashMap::from([(child, AgentStatus::Completed(Some("result".to_string())))]);
        let mut completed = ItemCompletedEvent {
            thread_id: parent,
            turn_id: "parent-turn".to_string(),
            item: TurnItem::CollabAgentToolCall(item.clone()),
            started_at_ms: Some(1),
            completed_at_ms: 2,
        };
        builder.handle_event(&EventMsg::ItemCompleted(completed.clone()));
        for mirror in EventMsg::ItemCompleted(completed.clone())
            .as_legacy_events(/*show_raw_agent_reasoning*/ false)
        {
            builder.handle_event(&mirror);
        }
        assert_eq!(
            builder
                .turn_snapshot("parent-turn")
                .expect("parent turn")
                .items,
            vec![ThreadItem::from(TurnItem::CollabAgentToolCall(
                item.clone()
            ))]
        );
        item.observe_commentary = None;
        item.wake_on_completion = None;
        item.target_messages = None;
        item.queue_input = None;
        completed.item = TurnItem::CollabAgentToolCall(item.clone());
        builder.handle_event(&EventMsg::ItemCompleted(completed));
        assert_eq!(
            builder.finish()[0].items,
            vec![ThreadItem::from(TurnItem::CollabAgentToolCall(item))]
        );
    }
}
