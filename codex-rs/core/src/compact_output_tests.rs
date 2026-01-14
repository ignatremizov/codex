use super::*;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use pretty_assertions::assert_eq;

fn message(text: &str) -> CompactedUserMessage {
    CompactedUserMessage {
        id: None,
        message: text.to_string(),
        internal_chat_message_metadata_passthrough: None,
        harness_metadata: None,
    }
}

#[test]
fn summary_presentation_excludes_metadata_and_preserves_unprefixed_text() {
    for (input, expected) in [
        (format!("{SUMMARY_PREFIX}\n  summary  "), Some("summary")),
        (format!("{SUMMARY_PREFIX}\n   "), None),
        ("summary".to_string(), Some("summary")),
        (
            format!("{SUMMARY_PREFIX}\nsummary\n\n[SESSION_METADATA]\nprivate path"),
            Some("summary"),
        ),
    ] {
        assert_eq!(summary_for_event(&input), expected.map(str::to_string));
    }
}

#[test]
fn output_limit_handles_missing_invalid_and_small_windows() {
    for (window, expected) in [
        (None, 136_000),
        (Some(-1), 136_000),
        (Some(0), 1),
        (Some(1), 1),
        (Some(32), 16),
        (Some(256), 128),
    ] {
        assert_eq!(compaction_output_token_limit(window), expected);
    }
}

#[test]
fn only_assistant_output_contributes_to_the_output_limit() {
    let text = "word ".repeat(100);
    for (role, expected) in [("assistant", approx_token_count(&text)), ("user", 0)] {
        let item = ResponseItem::Message {
            id: None,
            role: role.to_string(),
            content: vec![ContentItem::OutputText { text: text.clone() }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        assert_eq!(output_tokens_for_item(&item), expected);
    }
}

#[test]
fn metadata_contains_bounded_unicode_sizes_without_turn_contents() {
    let id = ThreadId::new();
    let messages = vec![message("short"), message(&"字".repeat(2_005))];
    assert_eq!(
        build_session_metadata_block(
            &id, /*rollout_path*/ None, &messages, /*recent_turns_in_prompt*/ 1
        ),
        format!(
            "[SESSION_METADATA]\nsession_id: {id}\nrollout_path: (unavailable)\nuser_turn_count: 2\nrecent_turns_in_prompt: 1\nlarge_user_turn_char_counts (threshold 2000, newest first):\nturn_index_from_end: 0, chars: 2005\n[/SESSION_METADATA]"
        )
    );
    assert_eq!(
        build_session_metadata_block(
            &id,
            Some(Path::new("history.jsonl")),
            &messages[..1],
            /*recent_turns_in_prompt*/ 1
        ),
        format!(
            "[SESSION_METADATA]\nsession_id: {id}\nrollout_path: history.jsonl\nuser_turn_count: 1\nrecent_turns_in_prompt: 1\n[/SESSION_METADATA]"
        )
    );
}
