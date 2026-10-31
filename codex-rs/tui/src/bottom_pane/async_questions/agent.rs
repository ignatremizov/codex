//! Bound questions follow the TUI view while their answers retain their requesting agent.

use super::*;
use crate::agent_question::AgentQuestionAnswerOutcome;
use crate::backend_id::BackendId;
use codex_protocol::ThreadId;

impl AsyncQuestions {
    pub(crate) fn has_pending_agent_answer(&self, answer: &AgentQuestionAnswer) -> bool {
        let origin = QuestionOrigin::Agent(answer.binding.clone());
        let key = origin.qualify(&answer.question_id);
        self.state.pending.iter().any(|question| {
            question.submission_pending && question.origin.qualify(&question.question_id) == key
        })
    }

    pub(crate) fn has_agent_question(
        &self,
        backend_id: Option<BackendId>,
        requester: ThreadId,
        message_id: &str,
    ) -> bool {
        self.state.pending.iter().any(|question| {
            question.message_id == message_id
                && matches!(&question.origin, QuestionOrigin::Agent(binding)
                    if Some(binding.backend_id) == backend_id
                        && binding.requester_thread_id == requester)
        })
    }

    pub(crate) fn take_agent_questions(&mut self) -> QuestionState {
        self.save_current_draft();
        let expanded = self.expanded
            && self
                .current_answer()
                .is_some_and(|question| matches!(question.origin, QuestionOrigin::Agent(_)));
        let current_idx = self
            .state
            .pending
            .iter()
            .take(self.state.current_idx)
            .filter(|question| matches!(question.origin, QuestionOrigin::Agent(_)))
            .count();
        let (pending, local): (Vec<_>, Vec<_>) = std::mem::take(&mut self.state.pending)
            .into_iter()
            .partition(|question| matches!(question.origin, QuestionOrigin::Agent(_)));
        self.state.pending = local;
        self.state.current_idx = 0;
        self.expanded = false;
        self.restore_current_draft();
        let (seen_ids, local_seen) = std::mem::take(&mut self.state.seen_ids)
            .into_iter()
            .partition(|id| id.starts_with("agent:"));
        self.state.seen_ids = local_seen;
        let (answered_ids, local_answered) = std::mem::take(&mut self.state.answered_ids)
            .into_iter()
            .partition(|id| id.starts_with("agent:"));
        self.state.answered_ids = local_answered;
        QuestionState {
            pending,
            current_idx,
            expanded,
            seen_ids,
            answered_ids,
        }
    }

    pub(crate) fn finish_agent_answer(
        &mut self,
        answer: &AgentQuestionAnswer,
        outcome: AgentQuestionAnswerOutcome,
    ) -> bool {
        let origin = QuestionOrigin::Agent(answer.binding.clone());
        let key = origin.qualify(&answer.question_id);
        match outcome {
            AgentQuestionAnswerOutcome::Accepted => self.resolve_answer_keys(&[key]),
            AgentQuestionAnswerOutcome::Rejected | AgentQuestionAnswerOutcome::Uncertain => {
                for question in &mut self.state.pending {
                    if question.origin.qualify(&question.question_id) == key {
                        question.submission_pending =
                            outcome == AgentQuestionAnswerOutcome::Uncertain;
                    }
                }
                false
            }
        }
    }

    pub(crate) fn resolve_agent_answers(
        &mut self,
        binding: AgentQuestionBinding,
        question_ids: &[String],
    ) -> bool {
        let origin = QuestionOrigin::Agent(binding);
        let keys = question_ids
            .iter()
            .map(|id| origin.qualify(id))
            .collect::<Vec<_>>();
        self.resolve_answer_keys(&keys)
    }
}
