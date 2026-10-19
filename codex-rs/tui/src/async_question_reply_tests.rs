use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn desktop_question_reply_accepts_single_and_batched_envelopes() {
    let reply = json!({"questionItemId": "[\"request_user_input_async\",\"item\",0]", "question": "Which environment?", "answer": "Staging", "extra": true});
    for payload in [reply.clone(), json!([reply])] {
        let text = format!(
            " \n<send_user_message_question_reply>\n{payload}\n</send_user_message_question_reply>\n "
        );
        assert_eq!(
            display_text(&text).as_deref(),
            Some("> Which environment?\n\nStaging")
        );
    }
    let text = "<send_user_message_question_reply>[{\"questionItemId\":\"one\",\"question\":\"First?\",\"answer\":\"Yes\"},{\"questionItemId\":\"two\",\"question\":\"Second?\",\"answer\":\"No\"}]</send_user_message_question_reply>";
    assert_eq!(
        display_text(text).as_deref(),
        Some("> First?\n\nYes\n\n> Second?\n\nNo")
    );
}

#[test]
fn numbered_replies_use_stable_references_in_single_batched_and_ide_envelopes() {
    let reply = json!({
        "questionItemId": "[\"request_user_input_async\",\"async-question:q3:call-a\",1]",
        "question": "Which environment should this lengthy question refer to?",
        "answer": "Staging\nKeep \"blue\""
    });
    for payload in [reply.clone(), json!([reply])] {
        let text = format!(
            "<send_user_message_question_reply>{payload}</send_user_message_question_reply>"
        );
        for text in [
            text.clone(),
            format!("# Context from my IDE setup:\n\n## My request for Codex:\n{text}"),
        ] {
            assert_eq!(
                display_text(&text).as_deref(),
                Some("q4: Staging\nKeep \"blue\"")
            );
        }
    }
    let text = "<send_user_message_question_reply>[{\"questionItemId\":\"[\\\"request_user_input_async\\\",\\\"async-question:q3:call-a\\\",1]\",\"question\":\"First?\",\"answer\":\"Yes\"},{\"questionItemId\":\"[\\\"request_user_input_async\\\",\\\"async-question:q1:call-b\\\",0]\",\"question\":\"Second?\",\"answer\":\"No\"}]</send_user_message_question_reply>";
    assert_eq!(display_text(text).as_deref(), Some("q4: Yes\n\nq1: No"));
}

#[test]
fn malformed_or_embedded_question_envelopes_remain_ordinary_text() {
    for text in [
        "> Which environment?\n\nStaging",
        "<send_user_message_question_reply>[]</send_user_message_question_reply>",
        "<send_user_message_question_reply>[{\"questionItemId\":\"one\",\"question\":\"First?\",\"answer\":\"Yes\"},null]</send_user_message_question_reply>",
        "Quoted: <send_user_message_question_reply>{\"questionItemId\":\"one\",\"question\":\"First?\",\"answer\":\"Yes\"}</send_user_message_question_reply>",
        "<send_user_message_question_reply>{\"questionItemId\":\"one\",\"question\":\"First?\",\"answer\":\"Yes\"}</send_user_message_question_reply> trailing text",
    ] {
        assert_eq!(parse(text), None, "{text}");
    }
}
