use pretty_assertions::assert_eq;

use super::ContextCompactionRender;
use super::context_compaction_render_plan;

#[test]
fn hidden_decode_error_renders_once_without_success_fallback() {
    assert_eq!(
        context_compaction_render_plan(
            Some("summary"),
            /*message*/ None,
            Some("decoder unavailable"),
            /*show_compact_summary*/ false,
        ),
        vec![ContextCompactionRender::DecodeError(
            "compacted prompt decoding failed: decoder unavailable".to_string()
        )]
    );
}

#[test]
fn visible_decode_error_precedes_available_summary() {
    assert_eq!(
        context_compaction_render_plan(
            Some("summary"),
            /*message*/ None,
            Some("decoder unavailable"),
            /*show_compact_summary*/ true,
        ),
        vec![
            ContextCompactionRender::DecodeError(
                "compacted prompt decoding failed: decoder unavailable".to_string()
            ),
            ContextCompactionRender::Section {
                title: "Compacted summary",
                content: "summary",
                empty_label: "(summary was empty)",
            },
        ]
    );
}

#[test]
fn absent_or_empty_decode_error_retains_success_fallback() {
    assert_eq!(
        [
            context_compaction_render_plan(
                /*summary*/ None, /*message*/ None, /*decode_error*/ None,
                /*show_compact_summary*/ false,
            ),
            context_compaction_render_plan(
                /*summary*/ None,
                /*message*/ None,
                Some("  "),
                /*show_compact_summary*/ true,
            ),
        ],
        [
            vec![ContextCompactionRender::Compacted],
            vec![ContextCompactionRender::Compacted],
        ]
    );
}
