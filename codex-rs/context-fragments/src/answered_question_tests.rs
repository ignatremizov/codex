//! Bounds, Unicode handling, and escaping for async question replies.

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn question_context_is_bounded_and_keeps_unicode_boundaries() {
    let text = "é\n".repeat(1_000);
    let id = r#"["request_user_input_async","message",1]"#;
    let answer = "A \"quoted\" answer\nwith a second line";
    let fragment = AnsweredQuestion::new(id, &text, answer);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&fragment.body()).unwrap(),
        serde_json::json!([{
            "questionItemId": id,
            "question": text[..text.floor_char_boundary(512)].replace('\n', " "),
            "answer": answer,
        }]),
    );
}

#[test]
fn numbered_answers_project_without_changing_canonical_transport() {
    let identity = r#"["request_user_input_async","async-question:q3:call-a",1]"#;
    let answer = "Needs adjustment\nKeep \"blue\"";
    let canonical = AnsweredQuestion::new(identity, "Which color?", answer).render();
    assert_eq!(
        compact_answered_question(&canonical),
        Some(format!("q4: {answer}")),
    );
    assert!(canonical.contains("Which color?"));
    assert_eq!(compact_answered_question("q4: Needs adjustment"), None);
    assert_eq!(
        compact_answered_question(&format!("{canonical}\nAlso do something else")),
        None,
    );
    assert_eq!(
        compact_answered_question(
            &AnsweredQuestion::new("legacy-item", "Which color?", answer).render()
        ),
        None,
    );
}

#[test]
fn numbered_batch_preserves_answer_order_and_rejects_ambiguous_ids() {
    let (start, end) = AnsweredQuestion::type_markers();
    let body = serde_json::json!([
        {"questionItemId": "[\"request_user_input_async\",\"async-question:q3:call-b\",1]", "answer": "B"},
        {"questionItemId": "[\"request_user_input_async\",\"async-question:q1:call-a\",0]", "answer": "A"}
    ]);
    assert_eq!(
        compact_answered_question(&format!("{start}{body}{end}")),
        Some("q4: B\nq1: A".to_string()),
    );
    for identity in [
        r#"["other_tool","async-question:q3:call-a",0]"#,
        r#"["request_user_input_async","async-question:q0:call-a",0]"#,
        r#"["request_user_input_async","async-question:q3",0]"#,
        r#"["request_user_input_async","async-question:q18446744073709551615:call-a",1]"#,
    ] {
        assert_eq!(
            compact_answered_question(&AnsweredQuestion::new(identity, "Question", "A").render()),
            None,
        );
    }
}
