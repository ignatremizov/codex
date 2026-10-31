//! Local questions retain drafts until answered or their turn ends; bound agent questions
//! survive unrelated turns and view changes. Replay identities are scoped to the requester.

use super::*;
use crate::agent_question::AgentQuestionDelivery;
use crate::history_cell::sanitize_user_text;
use codex_context_fragments::AnsweredQuestion;
use codex_context_fragments::ContextualUserFragment;
use codex_utils_string::take_bytes_at_char_boundary;

// Match the model-authored title budget used by AnsweredQuestion.
const MAX_RECOVERED_QUESTION_TITLE_BYTES: usize = 512;

impl AsyncQuestions {
    pub(crate) fn append(&mut self, message_id: &str, questions: &[AsyncUserInputQuestion]) {
        self.append_from(message_id, questions, QuestionOrigin::Local);
    }

    pub(crate) fn append_from(
        &mut self,
        message_id: &str,
        questions: &[AsyncUserInputQuestion],
        origin: QuestionOrigin,
    ) {
        if questions.is_empty() || !self.state.seen_ids.insert(origin.qualify(message_id)) {
            return;
        }
        let was_empty = self.state.pending.is_empty();
        let expires_at = (!self.expanded).then(|| Instant::now() + Duration::from_secs(30));
        for (index, question) in questions.iter().enumerate() {
            // Match the desktop's JSON.stringify([tool name, item id, question index]).
            let question_id =
                serde_json::json!(["request_user_input_async", message_id, index]).to_string();
            if self
                .state
                .answered_ids
                .contains(&origin.qualify(&question_id))
                || self
                    .state
                    .answered_ids
                    .contains(&origin.qualify(message_id))
            {
                continue;
            }
            // Bound work before cloning or wrapping model-authored suggestions.
            let question = AsyncUserInputQuestion {
                title: question.title.clone(),
                options: question.options.as_ref().map(|options| {
                    options
                        .iter()
                        .take(32)
                        .filter(|label| label.len() <= 512)
                        .cloned()
                        .collect()
                }),
            };
            let has_options = question
                .options
                .as_ref()
                .is_some_and(|options| !options.is_empty());
            let mut options_state = ScrollState::new();
            options_state.selected_idx = has_options.then_some(0);
            self.state.pending.push(PendingQuestion {
                message_id: message_id.into(),
                question_id,
                origin: origin.clone(),
                submission_pending: false,
                question,
                options_state,
                draft: ComposerDraft::default(),
                expires_at,
            });
        }
        if was_empty {
            self.state.current_idx = 0;
            self.restore_current_draft();
        }
    }

    pub(crate) fn set_expanded(&mut self, expanded: bool) {
        self.save_current_draft();
        self.expanded = expanded && !self.state.pending.is_empty();
        if self.expanded {
            self.snooze_auto_resolution();
        }
    }

    pub(crate) fn navigate(&mut self, forward: bool) -> bool {
        let next = if forward {
            self.state.current_idx.checked_add(1)
        } else {
            self.state.current_idx.checked_sub(1)
        };
        let Some(next) = next.filter(|&index| index < self.state.pending.len()) else {
            return false;
        };
        self.save_current_draft();
        self.visible_options.set((0, 0));
        self.state.current_idx = next;
        self.restore_current_draft();
        true
    }

    pub(super) fn snooze_auto_resolution(&mut self) {
        for question in &mut self.state.pending {
            question.expires_at = None;
        }
    }

    pub(super) fn timer_remaining(&self, now: Instant) -> Option<Duration> {
        self.state
            .pending
            .iter()
            .filter_map(|question| question.expires_at?.checked_duration_since(now))
            .filter(|remaining| !remaining.is_zero())
            .min()
    }

    pub(crate) fn countdown(&self, now: Instant) -> Option<String> {
        self.timer_remaining(now)
            .filter(|remaining| *remaining <= Duration::from_secs(20))
            .map(|remaining| format!("{}s", remaining.as_secs_f64().ceil() as u64))
    }

    pub(super) fn go_next_or_submit(&mut self) {
        self.save_current_draft();
        let Some(answer) = self.current_answer() else {
            return;
        };
        if answer.submission_pending
            || !self.delivery_enabled && matches!(answer.origin, QuestionOrigin::Local)
        {
            return;
        }
        let selected = answer
            .options_state
            .selected_idx
            .and_then(|index| answer.question.options.as_ref()?.get(index))
            .map(String::as_str)
            .unwrap_or_default();
        // Only a fully displayed model-authored option may become user authorization.
        let (first, count) = self.visible_options.get();
        let index = self.selected_option_index().unwrap_or(0);
        if !self.focus_is_notes() && !(first..first + count).contains(&index) {
            self.composer.show_footer_flash(
                "Expand terminal to read the entire option".into(),
                Duration::from_secs(5),
            );
            return;
        }
        let text = if self.focus_is_notes() {
            answer.draft.text_with_pending()
        } else {
            selected.to_string()
        };
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let reply = AnsweredQuestion::new(&answer.question_id, &answer.question.title, text)
            .render()
            .into_text()
            .unwrap_or_default();
        if reply.chars().count() > codex_protocol::user_input::MAX_USER_INPUT_TEXT_CHARS {
            self.composer.show_footer_flash(
                "Answer too long; shorten it before sending".into(),
                Duration::from_secs(5),
            );
        } else if let QuestionOrigin::Agent(binding) = &answer.origin {
            let submission = AgentQuestionAnswer {
                binding: binding.clone(),
                question_id: answer.question_id.clone(),
                text: reply,
                delivery: AgentQuestionDelivery::Direct,
            };
            if let Some(answer) = self.current_answer_mut() {
                answer.submission_pending = true;
            }
            self.submission = Some(QuestionSubmission::Agent(submission));
        } else {
            self.submission = Some(QuestionSubmission::Submit(reply));
        }
    }

    /// Return whether resolving these answers changed the expanded question's input target.
    pub(crate) fn resolve_answers(&mut self, question_ids: &[String]) -> bool {
        self.resolve_answer_keys(question_ids)
    }

    pub(super) fn resolve_answer_keys(&mut self, question_ids: &[String]) -> bool {
        // History can arrive before live questions or before restoring local drafts.
        self.state.answered_ids.extend(question_ids.iter().cloned());
        // Older desktop replies identify the whole source message instead of one question.
        let answered = |question: &PendingQuestion| {
            question_ids.contains(&question.origin.qualify(&question.question_id))
                || question_ids.contains(&question.origin.qualify(&question.message_id))
        };
        if !self.state.pending.iter().any(answered) {
            return false;
        }
        let current_answered = self.current_answer().is_some_and(answered);
        let displayed_answer_changed = self.expanded && current_answered;
        if current_answered {
            self.composer.flush_pending_input();
        }
        let current_idx = self
            .state
            .pending
            .iter()
            .take(self.state.current_idx)
            .filter(|question| !answered(question))
            .count();
        self.state.pending.retain(|question| !answered(question));
        self.state.current_idx = if current_idx < self.state.pending.len() {
            current_idx
        } else {
            0
        };
        self.expanded &= !self.state.pending.is_empty();
        self.visible_options.set((0, 0));
        if current_answered {
            self.restore_current_draft();
            self.composer.reset_vim_mode();
        }
        displayed_answer_changed
    }

    pub(crate) fn accept_answer(&mut self) {
        if self.state.pending.is_empty() {
            return;
        }
        self.composer.flush_pending_input();
        self.visible_options.set((0, 0));
        let answer = self.state.pending.remove(self.state.current_idx);
        self.state
            .answered_ids
            .insert(answer.origin.qualify(&answer.question_id));
        if self.state.current_idx >= self.state.pending.len() {
            self.state.current_idx = 0;
        }
        self.expanded &= !self.state.pending.is_empty();
        self.restore_current_draft();
        self.composer.reset_vim_mode();
    }

    pub(crate) fn clear_pending(&mut self) {
        self.save_current_draft();
        // A local prompt or turn end cannot dismiss a different agent's question.
        let retained_index = self
            .state
            .pending
            .iter()
            .take(self.state.current_idx)
            .filter(|question| matches!(question.origin, QuestionOrigin::Agent(_)))
            .count();
        let retained_current = self
            .current_answer()
            .is_some_and(|question| matches!(question.origin, QuestionOrigin::Agent(_)));
        self.state
            .pending
            .retain(|question| matches!(question.origin, QuestionOrigin::Agent(_)));
        self.state.current_idx = retained_index.min(self.state.pending.len().saturating_sub(1));
        self.expanded &= retained_current && !self.state.pending.is_empty();
        self.submission = None;
        self.visible_options.set((0, 0));
        self.restore_current_draft();
        self.composer.reset_vim_mode();
    }

    /// Recover unsent typed answers with their quoted questions before clearing pending questions.
    pub(crate) fn take_pending_drafts(&mut self) -> Vec<String> {
        self.save_current_draft();
        let drafts = self
            .state
            .pending
            .iter()
            .filter(|question| matches!(question.origin, QuestionOrigin::Local))
            .filter_map(PendingQuestion::recovered_draft)
            .collect();
        self.clear_pending();
        drafts
    }

    pub(crate) fn capture(&mut self) -> QuestionState {
        self.composer.cancel_history_search();
        self.save_current_draft();
        self.state.expanded = self.expanded;
        let pending = self
            .state
            .pending
            .iter()
            .filter(|question| matches!(question.origin, QuestionOrigin::Local))
            .cloned()
            .collect::<Vec<_>>();
        let current_idx = self
            .state
            .pending
            .iter()
            .take(self.state.current_idx)
            .filter(|question| matches!(question.origin, QuestionOrigin::Local))
            .count()
            .min(pending.len().saturating_sub(1));
        QuestionState {
            pending,
            current_idx,
            expanded: self.expanded
                && self
                    .current_answer()
                    .is_some_and(|question| matches!(question.origin, QuestionOrigin::Local)),
            seen_ids: self
                .state
                .seen_ids
                .iter()
                .filter(|id| !id.starts_with("agent:"))
                .cloned()
                .collect(),
            answered_ids: self
                .state
                .answered_ids
                .iter()
                .filter(|id| !id.starts_with("agent:"))
                .cloned()
                .collect(),
        }
    }

    pub(crate) fn restore(&mut self, saved: QuestionState) {
        self.visible_options.set((0, 0));
        let incoming = std::mem::replace(&mut self.state, saved);
        let mut answered_ids = incoming.answered_ids;
        answered_ids.extend(self.state.answered_ids.iter().cloned());
        self.state
            .pending
            .extend(incoming.pending.into_iter().filter(|question| {
                !self
                    .state
                    .seen_ids
                    .contains(&question.origin.qualify(&question.message_id))
            }));
        self.state.seen_ids.extend(incoming.seen_ids);
        self.expanded = self.state.expanded && !self.state.pending.is_empty();
        if self.expanded {
            self.snooze_auto_resolution();
        }
        self.restore_current_draft();
        self.resolve_answer_keys(&answered_ids.into_iter().collect::<Vec<_>>());
    }
}

impl PendingQuestion {
    fn recovered_draft(&self) -> Option<String> {
        let answer = self.draft.text_with_pending();
        let answer = answer.trim();
        if answer.is_empty() {
            return None;
        }

        let title = &self.question.title;
        let prefix = take_bytes_at_char_boundary(title, MAX_RECOVERED_QUESTION_TITLE_BYTES);
        let sanitized = sanitize_user_text(prefix.into());
        let mut quoted = textwrap::indent(&sanitized, "> ");
        if quoted.ends_with('\n') {
            quoted.pop();
        }
        if quoted.is_empty() {
            quoted.push_str("> ");
        }
        if prefix.len() < title.len() {
            quoted.push('…');
        }
        Some(format!("{quoted}\n\n{answer}"))
    }
}
