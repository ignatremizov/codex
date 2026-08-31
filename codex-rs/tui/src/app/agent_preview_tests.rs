use pretty_assertions::assert_eq;

use super::*;

#[test]
fn detailed_preview_retains_more_response_text_than_compact_preview() {
    let response = format!("begin {} end", "x".repeat(AGENT_DETAIL_PREVIEW_MAX_CHARS));

    let compact = compact_agent_preview(&response).expect("compact preview");
    let detailed = detailed_agent_preview(&response).expect("detailed preview");

    assert_eq!(compact.chars().count(), AGENT_PREVIEW_MAX_CHARS);
    assert_eq!(detailed.chars().count(), AGENT_DETAIL_PREVIEW_MAX_CHARS);
    assert!(compact.ends_with('…'));
    assert!(detailed.ends_with('…'));
    assert!(detailed.starts_with(compact.trim_end_matches('…')));
}
