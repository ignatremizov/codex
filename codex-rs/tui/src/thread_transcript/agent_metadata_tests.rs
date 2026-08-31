use super::*;
use crate::history_cell::HistoryCell;
use crate::multi_agents::background_commentary_history_cell_from_agent_message;
use crate::multi_agents::SpawnRequestSummary;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::models::MessagePhase;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;

#[test]
fn canonical_spawn_metadata_keeps_partial_settings_across_later_label_updates() {
    let model_thread = ThreadId::new();
    let effort_thread = ThreadId::new();
    let model_spawn: ThreadItem = serde_json::from_value(serde_json::json!({
        "type": "collabAgentToolCall", "id": "spawn-model", "tool": "spawnAgent",
        "status": "completed", "senderThreadId": ThreadId::new().to_string(),
        "receiverThreadIds": [model_thread.to_string()],
        "receiverAgents": [{"threadId": model_thread.to_string(), "agentNickname": "Model worker", "agentRole": "worker"}],
        "model": "recorded-model", "agentsStates": {},
    })).expect("canonical model-only spawn");
    let effort_spawn: ThreadItem = serde_json::from_value(serde_json::json!({
        "type": "userAgentControl", "id": "spawn-effort", "action": "spawn",
        "targetThreadId": effort_thread.to_string(), "nickname": "Effort worker",
        "reasoningEffort": "high", "resumedTarget": false, "status": "succeeded",
    })).expect("canonical effort-only spawn");
    let later_wait: ThreadItem = serde_json::from_value(serde_json::json!({
        "type": "collabAgentToolCall", "id": "wait", "tool": "wait",
        "status": "completed", "senderThreadId": ThreadId::new().to_string(),
        "receiverThreadIds": [model_thread.to_string()],
        "receiverAgents": [{"threadId": model_thread.to_string(), "agentNickname": "Current name"}],
        "agentsStates": {},
    })).expect("later nickname update");

    assert_eq!(
        collab_agent_metadata_from_items([&model_spawn, &effort_spawn, &later_wait]),
        HashMap::from([
            (model_thread, AgentMetadata {
                agent_nickname: Some("Current name".into()),
                agent_role: Some("worker".into()),
                spawn_request: Some(SpawnRequestSummary {
                    model: Some("recorded-model".into()), reasoning_effort: None,
                }),
            }),
            (effort_thread, AgentMetadata {
                agent_nickname: Some("Effort worker".into()), agent_role: None,
                spawn_request: Some(SpawnRequestSummary {
                    model: None, reasoning_effort: Some(ReasoningEffort::High),
                }),
            }),
        ]),
    );
}

#[test]
fn late_agent_metadata_updates_labels_without_losing_preview_or_raw_source() {
    let thread_id = ThreadId::new();
    let original = background_commentary_history_cell_from_agent_message(
        "legacy-commentary",
        &format!("Agent commentary from `{thread_id}`:\n\nfirst\nsecond\nlast"),
        Some(&MessagePhase::Commentary),
        /*agent_response_preview_lines*/ 2,
        |_| AgentMetadata::default(),
    )
    .expect("commentary row");
    let raw_details = original
        .raw_lines()
        .into_iter()
        .skip(/*n*/ 1)
        .collect::<Vec<_>>();
    let mut cells: TranscriptCells = vec![Arc::new(original)];
    let metadata = HashMap::from([(
        thread_id,
        AgentMetadata {
            agent_nickname: Some("Robie".into()),
            agent_role: Some("explorer".into()),
            spawn_request: None,
        },
    )]);
    refresh_collab_agent_labels(&mut cells, &metadata);
    assert_eq!(
        cells[0]
            .raw_lines()
            .into_iter()
            .skip(/*n*/ 1)
            .collect::<Vec<_>>(),
        raw_details,
    );
    assert_snapshot!(
        cells[0].display_lines(/*width*/ 80).iter().map(ToString::to_string).collect::<Vec<_>>().join("\n"),
        @r"
    • Robie [explorer] sends:
      └ first
        … +2 rows hidden
    "
    );
    let enriched = Arc::clone(&cells[0]);
    refresh_collab_agent_labels(
        &mut cells,
        &HashMap::from([(thread_id, AgentMetadata::default())]),
    );
    assert!(Arc::ptr_eq(&enriched, &cells[0]));
}
