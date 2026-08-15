use super::*;
use crate::history_cell::HistoryCell;
use crate::multi_agents::background_commentary_history_cell_from_agent_message;
use codex_protocol::models::MessagePhase;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;

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
