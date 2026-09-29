//! Async answers retain the desktop reply envelope for transport and replay.
//! Numbered questions have a compact model projection without repeating the question.

use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;
use serde::Deserialize;

/// Reserved presentation identity for a batch beginning with this question number.
pub fn async_question_item_id(first: u64, call_id: &str) -> String {
    format!("async-question:q{first}:{call_id}")
}

/// Reads the first question number from a host-assigned presentation identity.
pub fn async_question_number(item_id: &str) -> Option<u64> {
    let (number, call_id) = item_id.strip_prefix("async-question:q")?.split_once(':')?;
    if call_id.is_empty() {
        return None;
    }
    let number = number.parse().ok()?;
    (number > 0).then_some(number)
}

/// Projects a complete reply envelope only. Canonical client payloads stay unchanged.
/// Legacy question IDs and mixed user text are left intact rather than guessed at.
pub fn compact_answered_question(text: &str) -> Option<String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Reply {
        question_item_id: String,
        answer: String,
    }
    let (start, end) = AnsweredQuestion::type_markers();
    let body = text.trim().strip_prefix(start)?.strip_suffix(end)?;
    let replies: Vec<Reply> = serde_json::from_str(body).ok()?;
    if replies.is_empty() {
        return None;
    }
    replies
        .into_iter()
        .map(|reply| {
            let (tool, item_id, index): (String, String, u64) =
                serde_json::from_str(&reply.question_item_id).ok()?;
            if tool != "request_user_input_async" {
                return None;
            }
            let number = async_question_number(&item_id)?.checked_add(index)?;
            Some(format!("q{number}: {}", reply.answer))
        })
        .collect::<Option<Vec<_>>>()
        .map(|answers| answers.join("\n"))
}

/// Identifies an answered question without repeating an unbounded model-authored prompt.
pub struct AnsweredQuestion<'a> {
    question_id: Option<&'a str>,
    question: String,
    answer: &'a str,
}

impl<'a> AnsweredQuestion<'a> {
    pub fn new(question_id: &'a str, question: &str, answer: &'a str) -> Self {
        let end = question.floor_char_boundary(question.len().min(512));
        Self {
            question_id: (question_id.len() <= 512).then_some(question_id),
            question: question[..end].replace(['\n', '\r'], " "),
            answer,
        }
    }
}

impl ContextualUserFragment for AnsweredQuestion<'_> {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("user.answered_question".into())
    }
    fn role(&self) -> &'static str {
        "user"
    }
    fn markers(&self) -> (&'static str, &'static str) {
        if self.question_id.is_some() {
            Self::type_markers()
        } else {
            ("", "")
        }
    }
    fn type_markers() -> (&'static str, &'static str) {
        (
            "<send_user_message_question_reply>",
            "</send_user_message_question_reply>",
        )
    }
    fn body(&self) -> String {
        let Some(question_id) = self.question_id else {
            return format!("> {}\n\n{}", self.question, self.answer);
        };
        let replies = serde_json::json!([{
            "answer": self.answer,
            "question": self.question,
            "questionItemId": question_id,
        }]);
        format!("\n{replies}\n")
    }
}

#[cfg(test)]
#[path = "answered_question_tests.rs"]
mod tests;
