//! Keyboard interaction for `/agent` action, role, and target autocomplete.

use super::*;

impl ChatComposer {
    pub(super) fn handle_key_event_with_agent_target_popup(
        &mut self,
        key_event: KeyEvent,
    ) -> (InputResult, bool) {
        if self.handle_shortcut_overlay_key(&key_event) {
            return (InputResult::None, true);
        }
        self.footer.mode = reset_mode_after_activity(self.footer.mode);

        let ActivePopup::AgentTarget(popup) = &mut self.popups.active else {
            unreachable!();
        };

        let selected = match key_event {
            KeyEvent {
                code: KeyCode::Up, ..
            }
            | KeyEvent {
                code: KeyCode::Char('p'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                popup.move_up();
                return (InputResult::None, true);
            }
            KeyEvent {
                code: KeyCode::Down,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('n'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                popup.move_down();
                return (InputResult::None, true);
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                self.popups.dismissed_agent_target = self
                    .current_agent_target_completion()
                    .map(|completion| (completion.scope, completion.query));
                self.popups.active = ActivePopup::None;
                return (InputResult::None, true);
            }
            KeyEvent {
                code: KeyCode::Tab, ..
            }
            | KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => popup.selected_target(),
            input => return self.handle_input_basic(input),
        };

        let Some(target) = selected else {
            self.popups.active = ActivePopup::None;
            return if key_event.code == KeyCode::Enter {
                self.handle_key_event_without_popup(key_event)
            } else {
                (InputResult::None, true)
            };
        };
        let Some(completion) = self.current_agent_target_completion() else {
            self.popups.active = ActivePopup::None;
            return (InputResult::None, true);
        };
        let advances_to_action_target = completion.scope == AgentTargetCompletionScope::Any
            && is_agent_target_action(&target.selector);
        let advances_to_observation_mode = completion.scope
            == AgentTargetCompletionScope::ExistingTarget
            && completion.action == Some("observe");
        self.insert_agent_target(completion.range, &target.selector);
        self.popups.active = ActivePopup::None;
        if advances_to_action_target || advances_to_observation_mode {
            self.sync_popups();
        } else {
            self.popups.dismissed_agent_target = Some((completion.scope, target.selector));
        }
        (InputResult::None, true)
    }

    fn current_agent_target_completion(&self) -> Option<AgentTargetCompletion> {
        let text = self.draft.textarea.text();
        let first_line_end = text.find('\n').unwrap_or(text.len());
        let first_line = &text[..first_line_end];
        agent_target_completion(first_line, self.draft.textarea.cursor())
    }

    fn insert_agent_target(&mut self, range: Range<usize>, selector: &str) {
        let text = self.draft.textarea.text();
        let tail_starts_with_whitespace = text[range.end..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace);
        let replacement = if tail_starts_with_whitespace {
            selector.to_string()
        } else {
            format!("{selector} ")
        };
        let cursor = range.start + replacement.len();
        self.draft.textarea.replace_range(range, &replacement);
        self.draft.textarea.set_cursor(cursor);
    }
}

#[cfg(test)]
#[path = "agent_target_input_tests.rs"]
mod tests;
