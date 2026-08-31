use std::collections::HashMap;

use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_protocol::models::MessagePhase;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;

use super::*;

#[test]
fn summary_uses_latest_task_and_own_response_from_canonical_turns() {
    let mut store = ThreadEventStore::new(/*capacity*/ 4);
    store.set_turns(vec![
        turn(
            "turn-1",
            vec![
                user_message("user-1", "older task"),
                agent_message("assistant-1", "older response"),
            ],
        ),
        turn(
            "turn-2",
            vec![
                user_message("user-2", "  newest   task\nwith spacing  "),
                agent_message("assistant-2", " newest response "),
            ],
        ),
    ]);

    assert_eq!(
        AgentControlSummary::from_store(&store),
        AgentControlSummary {
            task_preview: Some("newest task with spacing".to_string()),
            response_preview: Some("newest response".to_string()),
            terminal_outcome: Some(AgentTerminalOutcome::Completed),
            ..Default::default()
        }
    );
}

#[test]
fn spawned_agent_settings_merge_user_control_and_v1_spawn_items() {
    let target_thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000123").expect("valid thread id");
    let v1_thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000124").expect("valid thread id");
    let model_only_thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000125").expect("valid thread id");
    let reasoning_only_thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000126").expect("valid thread id");
    let mut store = ThreadEventStore::new(/*capacity*/ 4);
    store.set_turns(vec![turn(
        "turn-1",
        vec![
            ThreadItem::UserAgentControl {
                id: "control-1".to_string(),
                action: UserAgentControlAction::Spawn,
                authored_selector: Some("reviewer".to_string()),
                target_thread_id: Some(target_thread_id.to_string()),
                previous_owner_session_id: None,
                new_owner_session_id: None,
                agent_ref: Some("2".to_string()),
                nickname: Some("Anscombe".to_string()),
                role: Some("reviewer".to_string()),
                model: Some("gpt-5.6-sol".to_string()),
                reasoning_effort: Some(ReasoningEffort::High),
                prompt_preview: None,
                resumed_target: false,
                fork_mode: Some(UserAgentForkMode::LastNTurns { turns: 3 }),
                observe_commentary: Some(false),
                final_response: None,
                target_messages: Some(false),
                queue_input: Some(false),
                status: UserAgentControlStatus::Succeeded,
                error: None,
            },
            ThreadItem::CollabAgentToolCall {
                id: "spawn-1".to_string(),
                tool: CollabAgentTool::SpawnAgent,
                status: CollabAgentToolCallStatus::Completed,
                observe_commentary: Some(false),
                wake_on_completion: Some(false),
                target_messages: Some(false),
                queue_input: Some(false),
                sender_thread_id: target_thread_id.to_string(),
                receiver_thread_ids: vec![v1_thread_id.to_string()],
                receiver_agents: Vec::new(),
                prompt: Some("Inspect the change.".to_string()),
                model: Some("gpt-5.6-luna".to_string()),
                reasoning_effort: Some(ReasoningEffort::Medium),
                agents_states: HashMap::new(),
            },
            ThreadItem::UserAgentControl {
                id: "control-model-only".to_string(),
                action: UserAgentControlAction::Spawn,
                authored_selector: Some("reviewer".to_string()),
                target_thread_id: Some(model_only_thread_id.to_string()),
                previous_owner_session_id: None,
                new_owner_session_id: None,
                agent_ref: Some("4".to_string()),
                nickname: Some("Hopper".to_string()),
                role: Some("reviewer".to_string()),
                model: Some("gpt-5.6-luna".to_string()),
                reasoning_effort: None,
                prompt_preview: None,
                resumed_target: false,
                fork_mode: Some(UserAgentForkMode::None),
                observe_commentary: Some(false),
                final_response: None,
                target_messages: Some(false),
                queue_input: Some(false),
                status: UserAgentControlStatus::Succeeded,
                error: None,
            },
            ThreadItem::UserAgentControl {
                id: "control-reasoning-only".to_string(),
                action: UserAgentControlAction::Spawn,
                authored_selector: Some("worker".to_string()),
                target_thread_id: Some(reasoning_only_thread_id.to_string()),
                previous_owner_session_id: None,
                new_owner_session_id: None,
                agent_ref: Some("5".to_string()),
                nickname: Some("Noether".to_string()),
                role: Some("worker".to_string()),
                model: None,
                reasoning_effort: Some(ReasoningEffort::Max),
                prompt_preview: None,
                resumed_target: false,
                fork_mode: Some(UserAgentForkMode::None),
                observe_commentary: Some(false),
                final_response: None,
                target_messages: Some(false),
                queue_input: Some(false),
                status: UserAgentControlStatus::Succeeded,
                error: None,
            },
        ],
    )]);

    assert_eq!(
        spawned_agent_settings(&store),
        HashMap::from([
            (
                target_thread_id,
                SpawnedAgentSettings {
                    fork_mode: Some(UserAgentForkMode::LastNTurns { turns: 3 }),
                    model_settings: Some(SpawnRequestSummary {
                        model: Some("gpt-5.6-sol".to_string()),
                        reasoning_effort: Some(ReasoningEffort::High),
                    }),
                },
            ),
            (
                v1_thread_id,
                SpawnedAgentSettings {
                    fork_mode: None,
                    model_settings: Some(SpawnRequestSummary {
                        model: Some("gpt-5.6-luna".to_string()),
                        reasoning_effort: Some(ReasoningEffort::Medium),
                    }),
                },
            ),
            (
                model_only_thread_id,
                SpawnedAgentSettings {
                    fork_mode: Some(UserAgentForkMode::None),
                    model_settings: Some(SpawnRequestSummary {
                        model: Some("gpt-5.6-luna".to_string()),
                        reasoning_effort: None,
                    }),
                },
            ),
            (
                reasoning_only_thread_id,
                SpawnedAgentSettings {
                    fork_mode: Some(UserAgentForkMode::None),
                    model_settings: Some(SpawnRequestSummary {
                        model: None,
                        reasoning_effort: Some(ReasoningEffort::Max),
                    }),
                },
            ),
        ])
    );
}

#[test]
fn running_elapsed_prefers_canonical_turn_start_time() {
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should follow the Unix epoch")
        .as_secs()
        .saturating_sub(5);
    let mut store = ThreadEventStore::new(/*capacity*/ 4);
    store.set_turns(vec![Turn {
        id: "turn-1".to_string(),
        items: Vec::new(),
        items_view: TurnItemsView::Full,
        status: TurnStatus::InProgress,
        error: None,
        started_at: Some(i64::try_from(started_at).expect("timestamp should fit in i64")),
        completed_at: None,
        duration_ms: None,
    }]);

    let summary = AgentControlSummary::from_store(&store);

    assert!(
        summary
            .running_for
            .is_some_and(|elapsed| elapsed.as_secs() >= 5)
    );
    assert_eq!(summary.terminal_outcome, None);
}

fn turn(id: &str, items: Vec<ThreadItem>) -> Turn {
    Turn {
        id: id.to_string(),
        items,
        items_view: TurnItemsView::Full,
        status: TurnStatus::Completed,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    }
}

fn user_message(id: &str, text: &str) -> ThreadItem {
    ThreadItem::UserMessage {
        id: id.to_string(),
        client_id: None,
        content: vec![UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }],
    }
}

fn agent_message(id: &str, text: &str) -> ThreadItem {
    ThreadItem::AgentMessage {
        id: id.to_string(),
        text: text.to_string(),
        phase: Some(MessagePhase::FinalAnswer),
        memory_citation: None,
        delivery: None,
        questions: None,
    }
}
