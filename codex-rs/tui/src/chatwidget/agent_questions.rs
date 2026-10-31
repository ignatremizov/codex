//! Keep requester-bound question editing separate from input to the displayed conversation.

use super::*;
use crate::agent_question::AgentQuestionAnswer;
use crate::agent_question::AgentQuestionAnswerOutcome;
use crate::agent_question::AgentQuestionBinding;
use crate::backend_id::BackendId;
use crate::bottom_pane::QuestionOrigin;
use codex_protocol::items::AsyncUserInputQuestion;

impl ChatWidget {
    pub(crate) fn set_question_backend(&mut self, backend_id: BackendId) {
        self.question_backend_id = Some(backend_id);
    }

    pub(crate) fn has_pending_agent_answer(&self, answer: &AgentQuestionAnswer) -> bool {
        self.bottom_pane
            .questions
            .as_deref()
            .is_some_and(|editor| editor.has_pending_agent_answer(answer))
    }

    pub(crate) fn add_agent_questions(
        &mut self,
        binding: AgentQuestionBinding,
        message_id: &str,
        questions: &[AsyncUserInputQuestion],
    ) {
        self.question_backend_id.get_or_insert(binding.backend_id);
        self.add_async_questions_from(message_id, questions, QuestionOrigin::Agent(binding));
    }

    pub(crate) fn inherit_agent_questions(&mut self, previous: &mut ChatWidget) {
        if self.question_backend_id.is_none() {
            self.question_backend_id = previous.question_backend_id;
        }
        if let Some(editor) = previous.bottom_pane.questions.as_deref_mut() {
            let saved = editor.take_agent_questions();
            self.bottom_pane.restore_questions(Some(saved));
        }
    }

    pub(crate) fn finish_agent_question_answer(
        &mut self,
        answer: &AgentQuestionAnswer,
        outcome: AgentQuestionAnswerOutcome,
    ) {
        if let Some(editor) = self.bottom_pane.questions.as_deref_mut() {
            let changed = editor.finish_agent_answer(answer, outcome);
            if changed && !self.bottom_pane.has_active_view() {
                self.bottom_pane.key_chord_reset_requested = true;
            }
        }
        self.refresh_pending_input_preview();
        self.request_redraw();
    }

    pub(crate) fn resolve_agent_question_answers(
        &mut self,
        binding: AgentQuestionBinding,
        question_ids: &[String],
    ) {
        if let Some(editor) = self.bottom_pane.questions.as_deref_mut() {
            let changed = editor.resolve_agent_answers(binding, question_ids);
            if changed && !self.bottom_pane.has_active_view() {
                self.bottom_pane.key_chord_reset_requested = true;
            }
        }
        self.refresh_pending_input_preview();
        self.request_redraw();
    }

    pub(super) fn resolve_displayed_question_answers(&mut self, question_ids: &[String]) -> bool {
        let editor = self.bottom_pane.question_editor();
        let local_changed = editor.resolve_answers(question_ids);
        let agent_changed = match (self.question_backend_id, self.thread_id) {
            (Some(backend_id), Some(requester_thread_id)) => editor.resolve_agent_answers(
                AgentQuestionBinding {
                    backend_id,
                    source_thread_id: requester_thread_id,
                    requester_thread_id,
                    requester_label: String::new(),
                },
                question_ids,
            ),
            (Some(_), None) | (None, Some(_)) | (None, None) => false,
        };
        local_changed || agent_changed
    }
}
