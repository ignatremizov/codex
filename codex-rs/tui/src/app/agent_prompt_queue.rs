//! Process-lifetime queued user prompts for active agent turns.

use std::collections::HashMap;

use codex_app_server_protocol::AgentResponseHandling;

use super::agent_observation_display::AgentResponseObservationBinding;
use super::agent_preview::compact_agent_preview;
use super::agent_prompt::AgentPromptAdmission;
use super::agent_prompt::AgentPromptSubmission;
use super::*;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;
use crate::chatwidget::UserMessage;
use crate::chatwidget::agent_command::AgentSelector;

const AGENT_PROMPT_QUEUE_VIEW_ID: &str = "agent-prompt-queue";
const AGENT_PROMPT_QUEUE_ACTIONS_VIEW_ID: &str = "agent-prompt-queue-actions";
#[derive(Clone, Debug, PartialEq)]
pub(super) struct QueuedAgentPrompt {
    id: Uuid,
    source_thread_id: ThreadId,
    target: String,
    authored_selector: String,
    target_thread_id: ThreadId,
    user_message: UserMessage,
    response_handling: Option<AgentResponseHandling>,
}

impl QueuedAgentPrompt {
    pub(super) fn preview(&self) -> String {
        queued_agent_prompt_preview(&self.user_message)
    }

    pub(super) fn response_label(&self) -> &'static str {
        response_handling_label(self.response_handling)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct QueuedAgentPromptRow {
    prompt_id: Uuid,
    preview: String,
    response: &'static str,
}

impl App {
    pub(super) async fn queue_agent_prompt_to_selector(
        &mut self,
        app_server: &mut AppServerSession,
        source_thread_id: ThreadId,
        selector: AgentSelector,
        user_message: UserMessage,
        response_handling: Option<AgentResponseHandling>,
    ) {
        let target = match selector.control_target() {
            Ok(target) => target,
            Err(message) => {
                self.chat_widget.add_error_message(message);
                return;
            }
        };
        let target_thread_id = match self.resolve_agent_selector(app_server, &selector).await {
            Ok(thread_id) => thread_id,
            Err(message) => {
                self.chat_widget.add_error_message(message);
                return;
            }
        };
        if target_thread_id == source_thread_id {
            self.chat_widget.add_error_message(
                "Queue input for the current agent with Tab in the normal composer.".to_string(),
            );
            return;
        }
        if self
            .chat_widget
            .user_inputs_from_message(&user_message)
            .is_empty()
        {
            self.chat_widget
                .add_error_message("The queued agent prompt is empty.".to_string());
            return;
        }

        let label = self
            .agent_prompt_availability(target_thread_id)
            .into_label()
            .unwrap_or_else(|| target_thread_id.to_string());
        match self
            .submit_agent_prompt_with_control(
                app_server,
                source_thread_id,
                target_thread_id,
                target.clone(),
                selector.authored().to_string(),
                user_message.clone(),
                response_handling,
                AgentPromptAdmission::Queued,
            )
            .await
        {
            AgentPromptSubmission::Admitted { .. } | AgentPromptSubmission::Rejected => return,
            AgentPromptSubmission::TargetActive => {}
        }

        self.queued_agent_prompts
            .entry(target_thread_id)
            .or_default()
            .push_back(QueuedAgentPrompt {
                id: Uuid::new_v4(),
                source_thread_id,
                target,
                authored_selector: selector.authored().to_string(),
                target_thread_id,
                user_message,
                response_handling,
            });
        self.chat_widget.add_info_message(
            format!("Queued user prompt for {label}."),
            /*hint*/ None,
        );

        let attachment = self
            .thread_event_channels
            .get(&target_thread_id)
            .map(ThreadEventChannel::attachment);
        let live_attached = match attachment {
            Some(ThreadEventAttachment::Live) => Ok(true),
            Some(ThreadEventAttachment::ReplayOnly) => self
                .resume_replay_only_thread(app_server, target_thread_id)
                .await
                .map(|()| true),
            None => {
                self.attach_live_thread_for_selection(app_server, target_thread_id)
                    .await
            }
        };
        match live_attached {
            Ok(true) => {}
            Ok(false) => self.chat_widget.add_error_message(format!(
                "Failed to watch {label}; the user prompt remains queued but may require a manual \
                 retry."
            )),
            Err(error) => self.chat_widget.add_error_message(format!(
                "Failed to watch {label}; the user prompt remains queued but may require a manual \
                 retry: {error:#}"
            )),
        }

        // Close the completion-vs-queue race: the target may have become idle after the first
        // liveness read but before the queued item was recorded. A second authoritative read either
        // observes the active turn or schedules admission without waiting for another notification.
        self.refresh_agent_picker_thread_liveness(app_server, target_thread_id)
            .await;
        if !self.agent_navigation.is_running(target_thread_id) {
            self.app_event_tx
                .send(AppEvent::DrainAgentPromptQueue { target_thread_id });
        }
    }

    pub(super) async fn drain_agent_prompt_queue(
        &mut self,
        app_server: &mut AppServerSession,
        target_thread_id: ThreadId,
    ) {
        if !self.queued_agent_prompts.contains_key(&target_thread_id) {
            return;
        }
        self.refresh_agent_picker_thread_liveness(app_server, target_thread_id)
            .await;
        self.sync_active_agent_label();
        if self.agent_navigation.is_running(target_thread_id) {
            return;
        }

        let Some(prompt) = self
            .queued_agent_prompts
            .get(&target_thread_id)
            .and_then(|queue| queue.front())
            .cloned()
        else {
            return;
        };
        let label = self
            .agent_prompt_availability(target_thread_id)
            .into_label()
            .unwrap_or_else(|| target_thread_id.to_string());
        let items = self
            .chat_widget
            .user_inputs_from_message(&prompt.user_message);
        match self
            .submit_agent_prompt_items(
                app_server,
                prompt.source_thread_id,
                prompt.target_thread_id,
                prompt.target,
                prompt.authored_selector,
                items,
                prompt.response_handling,
                AgentPromptAdmission::Queued,
            )
            .await
        {
            Ok(
                outcome @ AgentPromptSubmission::Admitted {
                    audit_warning: _,
                    post_admission_warning: _,
                },
            ) => {
                let AgentPromptSubmission::Admitted {
                    audit_warning,
                    post_admission_warning,
                } = &outcome
                else {
                    unreachable!("matched admitted outcome above")
                };
                self.take_queued_agent_prompt(target_thread_id, prompt.id);
                self.refresh_primary_agent_aliases(app_server).await;
                self.refresh_agent_picker_thread_liveness(app_server, prompt.target_thread_id)
                    .await;
                if post_admission_warning.is_none()
                    && self.agent_navigation.is_running(prompt.target_thread_id)
                {
                    self.agent_navigation.note_response_observation(
                        prompt.source_thread_id,
                        prompt.target_thread_id,
                        AgentResponseObservationBinding::Bound,
                        prompt.response_handling,
                    );
                }
                self.sync_active_agent_label();
                if let Some(warning) = audit_warning {
                    self.chat_widget.add_error_message(format!(
                        "Queued prompt was admitted to {label}, but its source audit failed; it \
                         was removed from the queue and must not be retried: {warning}"
                    ));
                }
                if let Some(warning) = post_admission_warning {
                    self.chat_widget.add_error_message(format!(
                        "Queued prompt was admitted to {label}, but response handling degraded; it \
                         was removed from the queue and must not be retried: {warning}"
                    ));
                }
            }
            Ok(AgentPromptSubmission::TargetActive) => {}
            Ok(AgentPromptSubmission::Rejected) => {
                unreachable!("submission helper never returns rejected")
            }
            Err(error) => {
                tracing::warn!(
                    target_thread_id = %prompt.target_thread_id,
                    %error,
                    "failed to submit queued user prompt to agent; retaining it for retry"
                );
                self.chat_widget.add_error_message(format!(
                    "Failed to send queued user prompt to {label}; it remains queued: {error:#}"
                ));
            }
        }
    }

    pub(super) fn open_agent_prompt_queue(&mut self, target_thread_id: ThreadId) {
        let selected = self
            .chat_widget
            .selected_index_for_present_view(AGENT_PROMPT_QUEUE_VIEW_ID);
        let params = self.agent_prompt_queue_view_params(target_thread_id, selected);
        if !self
            .chat_widget
            .replace_selection_view_if_present(AGENT_PROMPT_QUEUE_VIEW_ID, params)
        {
            let params = self.agent_prompt_queue_view_params(target_thread_id, selected);
            self.chat_widget.show_selection_view(params);
        }
    }

    pub(super) fn edit_queued_agent_prompt(&mut self, target_thread_id: ThreadId, prompt_id: Uuid) {
        let Some(prompt) = self.take_queued_agent_prompt(target_thread_id, prompt_id) else {
            self.chat_widget
                .add_error_message("That queued agent prompt is no longer available.".to_string());
            return;
        };
        let response_option = response_handling_option(prompt.response_handling)
            .map(|option| format!(" {option}"))
            .unwrap_or_default();
        let command = format!(
            "/agent queue {}{response_option} ",
            prompt.authored_selector
        );
        self.chat_widget
            .dismiss_selection_view(AGENT_PROMPT_QUEUE_VIEW_ID);
        self.chat_widget
            .restore_user_message_to_composer(prompt.user_message);
        self.chat_widget
            .restore_user_message_to_composer(UserMessage::from(command));
    }

    pub(super) fn remove_queued_agent_prompt(
        &mut self,
        target_thread_id: ThreadId,
        prompt_id: Uuid,
    ) {
        if self
            .take_queued_agent_prompt(target_thread_id, prompt_id)
            .is_none()
        {
            self.chat_widget
                .add_error_message("That queued agent prompt is no longer available.".to_string());
        }
        self.open_agent_prompt_queue(target_thread_id);
    }

    pub(super) fn clear_agent_prompt_queues_for_thread(&mut self, thread_id: ThreadId) {
        remove_queued_agent_prompts_for_thread(&mut self.queued_agent_prompts, thread_id);
    }

    pub(super) fn open_queued_agent_prompt_actions(
        &mut self,
        target_thread_id: ThreadId,
        prompt_id: Uuid,
    ) {
        let Some(prompt) = self
            .queued_agent_prompts
            .get(&target_thread_id)
            .and_then(|queue| queue.iter().find(|prompt| prompt.id == prompt_id))
        else {
            self.chat_widget
                .add_error_message("That queued agent prompt is no longer available.".to_string());
            return;
        };
        let preview = prompt.preview();
        self.chat_widget
            .show_selection_view(queued_agent_prompt_actions_view_params(
                target_thread_id,
                prompt_id,
                preview,
            ));
    }

    fn agent_prompt_queue_view_params(
        &self,
        target_thread_id: ThreadId,
        initial_selected_idx: Option<usize>,
    ) -> SelectionViewParams {
        let label = self
            .agent_navigation
            .display_name(target_thread_id, self.agent_root_thread_id());
        let queued = queued_agent_prompt_rows(self.queued_agent_prompts.get(&target_thread_id));
        agent_prompt_queue_view_params(target_thread_id, label, queued, initial_selected_idx)
    }

    fn take_queued_agent_prompt(
        &mut self,
        target_thread_id: ThreadId,
        prompt_id: Uuid,
    ) -> Option<QueuedAgentPrompt> {
        let queue = self.queued_agent_prompts.get_mut(&target_thread_id)?;
        let prompt = take_queued_agent_prompt_by_id(queue, prompt_id);
        if queue.is_empty() {
            self.queued_agent_prompts.remove(&target_thread_id);
        }
        prompt
    }
}

fn queued_agent_prompt_actions_view_params(
    target_thread_id: ThreadId,
    prompt_id: Uuid,
    preview: String,
) -> SelectionViewParams {
    SelectionViewParams {
        view_id: Some(AGENT_PROMPT_QUEUE_ACTIONS_VIEW_ID),
        title: Some("Queued follow-up".to_string()),
        subtitle: Some(preview),
        footer_hint: Some(standard_popup_hint_line()),
        items: vec![
            SelectionItem {
                name: "Edit".to_string(),
                description: Some("Restore this prompt to the composer".to_string()),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::EditQueuedAgentPrompt {
                        target_thread_id,
                        prompt_id,
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "Remove".to_string(),
                description: Some("Delete this queued prompt".to_string()),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::RemoveQueuedAgentPrompt {
                        target_thread_id,
                        prompt_id,
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}

fn agent_prompt_queue_view_params(
    target_thread_id: ThreadId,
    label: String,
    queued: Vec<QueuedAgentPromptRow>,
    initial_selected_idx: Option<usize>,
) -> SelectionViewParams {
    let items = if queued.is_empty() {
        vec![SelectionItem {
            name: "No queued follow-ups".to_string(),
            disabled_reason: Some("Add one with `/agent queue <target> <prompt>`.".to_string()),
            ..Default::default()
        }]
    } else {
        queued
            .into_iter()
            .map(|row| {
                let QueuedAgentPromptRow {
                    prompt_id,
                    preview,
                    response,
                } = row;
                SelectionItem {
                    name: preview.clone(),
                    description: Some(response.to_string()),
                    search_value: Some(preview),
                    secondary_action: Some(Box::new(move |tx| {
                        tx.send(AppEvent::OpenQueuedAgentPromptActions {
                            target_thread_id,
                            prompt_id,
                        });
                    })),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::EditQueuedAgentPrompt {
                            target_thread_id,
                            prompt_id,
                        });
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                }
            })
            .collect()
    };

    SelectionViewParams {
        view_id: Some(AGENT_PROMPT_QUEUE_VIEW_ID),
        title: Some(format!("Queued for {label}")),
        subtitle: Some(target_thread_id.to_string()),
        footer_note: Some("Enter edits · Tab opens actions".into()),
        footer_hint: Some(standard_popup_hint_line()),
        items,
        initial_selected_idx,
        ..Default::default()
    }
}

fn take_queued_agent_prompt_by_id(
    queue: &mut VecDeque<QueuedAgentPrompt>,
    prompt_id: Uuid,
) -> Option<QueuedAgentPrompt> {
    let index = queue.iter().position(|prompt| prompt.id == prompt_id)?;
    queue.remove(index)
}

fn remove_queued_agent_prompts_for_thread(
    queues: &mut HashMap<ThreadId, VecDeque<QueuedAgentPrompt>>,
    thread_id: ThreadId,
) {
    queues.remove(&thread_id);
    queues.retain(|_, queue| {
        queue.retain(|prompt| prompt.source_thread_id != thread_id);
        !queue.is_empty()
    });
}

fn queued_agent_prompt_rows(
    queued: Option<&VecDeque<QueuedAgentPrompt>>,
) -> Vec<QueuedAgentPromptRow> {
    queued
        .map(VecDeque::iter)
        .into_iter()
        .flatten()
        .map(|prompt| QueuedAgentPromptRow {
            prompt_id: prompt.id,
            preview: prompt.preview(),
            response: prompt.response_label(),
        })
        .collect()
}

fn response_handling_option(
    response_handling: Option<AgentResponseHandling>,
) -> Option<&'static str> {
    response_handling.map(|response_handling| match response_handling {
        AgentResponseHandling::Commentary => "w:c",
        AgentResponseHandling::Wake => "w:f",
        AgentResponseHandling::Presentation => "w:x",
        AgentResponseHandling::CommentaryWake => "w:cf",
        AgentResponseHandling::CommentaryPresentation => "w:cx",
    })
}

fn response_handling_label(response_handling: Option<AgentResponseHandling>) -> &'static str {
    response_handling_option(response_handling).unwrap_or("passive")
}

fn queued_agent_prompt_preview(user_message: &UserMessage) -> String {
    if let Some(preview) = compact_agent_preview(&user_message.text) {
        return preview;
    }
    let attachment_count = user_message.local_images.len() + user_message.remote_image_urls.len();
    match attachment_count {
        0 => "Structured input".to_string(),
        1 => "1 image".to_string(),
        count => format!("{count} images"),
    }
}

#[cfg(test)]
#[path = "agent_prompt_queue_tests.rs"]
mod tests;
