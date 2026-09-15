use super::*;
use crate::protocol::v2::ThreadItem;
use codex_protocol::CollabAgentInputBatch;
use codex_protocol::CollabAgentInputResult;
use codex_protocol::CollabAgentInputStatus;
use codex_protocol::ThreadId;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::CollabAgentRef;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HasLegacyEvent;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::TurnStartedEvent;
use pretty_assertions::assert_eq;
use std::collections::HashMap;

#[test]
fn legacy_mirrors_preserve_live_policy_but_do_not_override_a_new_canonical_item() {
    for (tool, batched) in [
        (CollabAgentTool::SpawnAgent, false),
        (CollabAgentTool::SendInput, false),
        (CollabAgentTool::ResumeAgent, false),
        (CollabAgentTool::SendInput, true),
    ] {
        let parent = ThreadId::new();
        let child = ThreadId::new();
        let mut item = CollabAgentToolCallItem {
            input_batch: batched.then(|| CollabAgentInputBatch {
                flags: "zf".to_string(),
                sender_thread_id: Some(parent),
                results: Vec::new(),
            }),
            mailbox_input: Some(true),
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
            receiver_agents: if matches!(tool, CollabAgentTool::SpawnAgent) {
                vec![CollabAgentRef {
                    thread_id: child,
                    agent_ref: None,
                    task_path: None,
                    agent_nickname: Some("child agent".to_string()),
                    agent_role: Some("worker".to_string()),
                }]
            } else {
                Vec::new()
            },
            prompt: (!matches!(tool, CollabAgentTool::ResumeAgent)).then(|| "task".to_string()),
            model: matches!(tool, CollabAgentTool::SpawnAgent).then(|| "test-model".to_string()),
            reasoning_effort: matches!(tool, CollabAgentTool::SpawnAgent)
                .then_some(ReasoningEffort::High),
            agents_states: HashMap::from([(child, AgentStatus::Running)]),
            completion_presentation_agent_ids: None,
        };
        let mut builder = ThreadHistoryBuilder::new();
        builder.handle_event(&EventMsg::TurnStarted(TurnStartedEvent {
            turn_attribution: None,
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
        if batched {
            assert!(
                started
                    .as_legacy_events(/*show_raw_agent_reasoning*/ false)
                    .is_empty()
            );
        }
        // Model an older compatibility sender that omitted the batch metadata.
        // A canonical batch intentionally has no single-receiver legacy fanout.
        let mut legacy_started = started.clone();
        let EventMsg::ItemStarted(ItemStartedEvent {
            item: TurnItem::CollabAgentToolCall(legacy_item),
            ..
        }) = &mut legacy_started
        else {
            panic!("expected collab item start");
        };
        legacy_item.input_batch = None;
        let mirrors = legacy_started.as_legacy_events(/*show_raw_agent_reasoning*/ false);
        assert!(!mirrors.is_empty());
        for mirror in mirrors {
            builder.handle_event(&mirror);
        }
        if batched {
            let snapshot = builder.turn_snapshot("parent-turn").expect("parent turn");
            let ThreadItemPayload::CollabAgentToolCall { input_batch, .. } =
                &snapshot.items[0].payload
            else {
                panic!("expected collab item");
            };
            assert_eq!(input_batch, &item.input_batch);
            item.input_batch.as_mut().expect("batch").results = vec![
                CollabAgentInputResult {
                    target: "2".to_string(),
                    receiver_thread_id: Some(child.to_string()),
                    status: CollabAgentInputStatus::MailboxAccepted,
                    error: None,
                    hint: None,
                },
                CollabAgentInputResult {
                    target: "unknown".to_string(),
                    receiver_thread_id: None,
                    status: CollabAgentInputStatus::Error,
                    error: Some("Unknown selector.".to_string()),
                    hint: Some("Use a current agent reference.".to_string()),
                },
            ];
            item.status = CollabAgentToolCallStatus::Failed;
            item.agents_states = HashMap::from([(
                child,
                AgentStatus::Errored("receiver failed after admission".to_string()),
            )]);
        } else {
            item.status = CollabAgentToolCallStatus::Completed;
            item.agents_states =
                HashMap::from([(child, AgentStatus::Completed(Some("result".to_string())))]);
        }
        let mut completed = ItemCompletedEvent {
            thread_id: parent,
            turn_id: "parent-turn".to_string(),
            item: TurnItem::CollabAgentToolCall(item.clone()),
            started_at_ms: Some(1),
            completed_at_ms: 2,
        };
        builder.handle_event(&EventMsg::ItemCompleted(completed.clone()));
        if batched {
            assert!(
                EventMsg::ItemCompleted(completed.clone())
                    .as_legacy_events(/*show_raw_agent_reasoning*/ false)
                    .is_empty()
            );
        }
        let mut legacy_completed = completed.clone();
        let TurnItem::CollabAgentToolCall(legacy_item) = &mut legacy_completed.item else {
            panic!("expected collab item completion");
        };
        legacy_item.input_batch = None;
        let mirrors = EventMsg::ItemCompleted(legacy_completed)
            .as_legacy_events(/*show_raw_agent_reasoning*/ false);
        assert!(!mirrors.is_empty());
        for mirror in mirrors {
            builder.handle_event(&mirror);
        }
        assert_eq!(
            builder
                .turn_snapshot("parent-turn")
                .expect("parent turn")
                .items,
            vec![ThreadItem {
                payload: ThreadItemPayload::from(TurnItem::CollabAgentToolCall(item.clone())),
                started_at_ms: Some(1),
                completed_at_ms: Some(2),
            }]
        );
        item.observe_commentary = None;
        item.wake_on_completion = None;
        item.target_messages = None;
        item.queue_input = None;
        item.mailbox_input = None;
        item.input_batch = None;
        completed.item = TurnItem::CollabAgentToolCall(item.clone());
        builder.handle_event(&EventMsg::ItemCompleted(completed));
        assert_eq!(
            builder.finish()[0].items,
            vec![ThreadItem {
                payload: ThreadItemPayload::from(TurnItem::CollabAgentToolCall(item)),
                started_at_ms: Some(1),
                completed_at_ms: Some(2),
            }]
        );
    }
}
