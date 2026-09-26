use super::*;
use crate::history_cell::HistoryCell;
use crate::multi_agents::background_commentary_history_cell_from_agent_message;
use codex_app_server_protocol::ThreadItem;
use codex_protocol::models::MessagePhase;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;

#[test]
fn lifecycle_mapping_preserves_agent_ref_and_authoritative_task_clear() {
    let target = codex_protocol::ThreadId::new();
    let item = ThreadItem::UserAgentControl {
        id: "resume-1".to_string(),
        input_outcome: None,
        action: codex_app_server_protocol::UserAgentControlAction::Resume,
        authored_selector: None,
        target_thread_id: Some(target.to_string()),
        observer_thread_id: None,
        authored_observer_selector: None,
        reply_recipient_thread_id: None,
        previous_owner_session_id: None,
        new_owner_session_id: None,
        agent_ref: Some("5".to_string()),
        nickname: Some("Darwin".to_string()),
        role: None,
        task: None,
        task_path: Some("/root/previous".to_string()),
        task_path_mapping: vec![codex_app_server_protocol::AgentTaskPathMapping {
            thread_id: target.to_string(),
            previous_task_path: Some("/root/previous".to_string()),
            task_path: None,
        }],
        model: None,
        reasoning_effort: None,
        prompt_preview: None,
        resumed_target: true,
        fork_mode: None,
        observe_commentary: None,
        final_response: None,
        target_messages: None,
        queue_input: None,
        status: codex_app_server_protocol::UserAgentControlStatus::Succeeded,
        error: None,
    };

    assert_eq!(
        collab_agent_metadata_from_items([&item]),
        HashMap::from([(
            target,
            AgentMetadata {
                agent_ref: Some("5".to_string()),
                agent_nickname: Some("Darwin".to_string()),
                task_path: AgentTaskPath::Known(None),
                ..Default::default()
            }
        )])
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
            ..Default::default()
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
