//! Process-lifetime queued user prompts for distinct future agent turns.

use codex_app_server_protocol::AgentQueueEntry;
use codex_app_server_protocol::AgentResponseHandling;
use codex_protocol::models::local_image_label_text;

use super::agent_preview::compact_agent_preview;
use super::agent_prompt::AgentPromptAdmission;
use super::agent_prompt::AgentPromptSubmission;
use super::*;
use crate::bottom_pane::LocalImageAttachment;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;
use crate::chatwidget::UserMessage;
use crate::chatwidget::agent_command::AgentSelector;
use crate::chatwidget::mention_bindings_from_user_inputs;

const AGENT_PROMPT_QUEUE_VIEW_ID: &str = "agent-prompt-queue";
const AGENT_PROMPT_QUEUE_ACTIONS_VIEW_ID: &str = "agent-prompt-queue-actions";
#[derive(Clone, Debug, PartialEq)]
pub(super) struct QueuedAgentPrompt {
    id: Uuid,
    source_thread_id: ThreadId,
    authored_selector: Option<String>,
    target_thread_id: ThreadId,
    user_message: UserMessage,
    preview: String,
    response_handling: Option<AgentResponseHandling>,
}

impl QueuedAgentPrompt {
    pub(super) fn preview(&self) -> String {
        self.preview.clone()
    }

    pub(super) fn response_label(&self) -> String {
        response_handling_option(self.response_handling).unwrap_or_else(|| "passive".to_string())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct QueuedAgentPromptRow {
    prompt_id: Uuid,
    preview: String,
    response: String,
}

impl App {
    pub(super) fn apply_primary_agent_queue(&mut self, queued: Vec<AgentQueueEntry>) {
        self.queued_agent_prompts.clear();
        for entry in queued {
            let Some(prompt) = queued_agent_prompt_from_entry(entry) else {
                continue;
            };
            self.queued_agent_prompts
                .entry(prompt.target_thread_id)
                .or_default()
                .push_back(prompt);
        }
    }

    pub(super) async fn refresh_primary_agent_queue(&mut self, app_server: &AppServerSession) {
        let Some(primary_thread_id) = self.primary_thread_id else {
            return;
        };
        match app_server.agent_queued_turns(primary_thread_id).await {
            Ok(queued) => self.apply_primary_agent_queue(queued),
            Err(err) => tracing::warn!(%err, "failed to refresh queued agent turns"),
        }
    }

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
        let outcome = self
            .submit_agent_prompt_with_control(
                app_server,
                SubmitAgentPromptArgs {
                    source_thread_id,
                    thread_id: target_thread_id,
                    target: target.clone(),
                    authored_selector: selector.authored().to_string(),
                    user_message: user_message.clone(),
                    response_handling,
                    admission: AgentPromptAdmission::Queued,
                },
            )
            .await;
        match outcome {
            AgentPromptSubmission::Admitted { .. } => {
                self.refresh_primary_agent_queue(app_server).await;
                self.chat_widget.add_info_message(
                    format!("Submitted queued turn for {label}."),
                    /*hint*/ None,
                );
            }
            AgentPromptSubmission::Rejected => {}
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

    pub(super) async fn edit_queued_agent_prompt(
        &mut self,
        app_server: &AppServerSession,
        target_thread_id: ThreadId,
        prompt_id: Uuid,
    ) {
        let Some(prompt) = self
            .queued_agent_prompts
            .get(&target_thread_id)
            .and_then(|queue| queue.iter().find(|prompt| prompt.id == prompt_id))
            .cloned()
        else {
            self.chat_widget
                .add_error_message("That queued agent prompt is no longer available.".to_string());
            return;
        };
        let Some(authored_selector) = prompt.authored_selector.as_deref() else {
            self.chat_widget.add_error_message(
                "Model-authored queued input can be removed, but not edited as user input."
                    .to_string(),
            );
            return;
        };
        let Some(primary_thread_id) = self.primary_thread_id else {
            self.chat_widget
                .add_error_message("No primary agent queue is available.".to_string());
            return;
        };
        if let Err(err) = app_server
            .delete_agent_queued_turn(primary_thread_id, prompt_id)
            .await
        {
            self.chat_widget.add_error_message(format!(
                "Failed to remove the queued prompt before editing it: {err:#}"
            ));
            self.refresh_primary_agent_queue(app_server).await;
            return;
        }
        let _ = self.take_queued_agent_prompt(target_thread_id, prompt_id);
        let response_option = response_handling_option(prompt.response_handling)
            .map(|option| format!(" {option}"))
            .unwrap_or_default();
        let command = format!("/agent queue {authored_selector}{response_option} ");
        self.chat_widget
            .dismiss_selection_view(AGENT_PROMPT_QUEUE_VIEW_ID);
        self.chat_widget
            .restore_user_message_to_composer(prompt.user_message);
        self.chat_widget
            .restore_user_message_to_composer(UserMessage::from(command));
    }

    pub(super) async fn remove_queued_agent_prompt(
        &mut self,
        app_server: &AppServerSession,
        target_thread_id: ThreadId,
        prompt_id: Uuid,
    ) {
        let Some(primary_thread_id) = self.primary_thread_id else {
            self.chat_widget
                .add_error_message("No primary agent queue is available.".to_string());
            return;
        };
        match app_server
            .delete_agent_queued_turn(primary_thread_id, prompt_id)
            .await
        {
            Ok(()) => {
                let _ = self.take_queued_agent_prompt(target_thread_id, prompt_id);
            }
            Err(err) => {
                self.chat_widget
                    .add_error_message(format!("Failed to remove queued agent input: {err:#}"));
                self.refresh_primary_agent_queue(app_server).await;
            }
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
        let editable = prompt.authored_selector.is_some();
        self.chat_widget
            .show_selection_view(queued_agent_prompt_actions_view_params(
                target_thread_id,
                prompt_id,
                preview,
                editable,
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
    editable: bool,
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
                disabled_reason: (!editable)
                    .then(|| "Only user-authored queued prompts can be edited.".to_string()),
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
                    description: Some(response),
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

fn response_handling_option(response_handling: Option<AgentResponseHandling>) -> Option<String> {
    let response_handling = response_handling?;
    let mut flags = String::new();
    if response_handling.commentary {
        flags.push('c');
    }
    if response_handling.final_response
        == codex_app_server_protocol::AgentFinalResponseHandling::Wake
    {
        flags.push('f');
    }
    if response_handling.target_messages {
        flags.push('m');
    }
    if response_handling.queue_input {
        flags.push('q');
    }
    if response_handling.final_response
        == codex_app_server_protocol::AgentFinalResponseHandling::Presentation
    {
        flags.push('x');
    }
    (!flags.is_empty()).then(|| format!("w:{flags}"))
}

fn queued_agent_prompt_from_entry(entry: AgentQueueEntry) -> Option<QueuedAgentPrompt> {
    let id = Uuid::parse_str(&entry.id).ok()?;
    let source_thread_id = ThreadId::from_string(&entry.source_thread_id).ok()?;
    let target_thread_id = ThreadId::from_string(&entry.target_thread_id).ok()?;
    let display = ChatWidget::user_message_display_from_inputs(&entry.input);
    let local_images = display
        .local_images
        .into_iter()
        .enumerate()
        .map(|(index, path)| LocalImageAttachment {
            placeholder: local_image_label_text(index + 1),
            path,
        })
        .collect();
    let mention_bindings = mention_bindings_from_user_inputs(&entry.input, &display.message);
    let user_message = UserMessage {
        text: display.message,
        local_images,
        remote_image_urls: display.remote_image_urls,
        text_elements: display.text_elements,
        mention_bindings,
    };
    let preview = compact_agent_preview(&entry.prompt_preview)
        .unwrap_or_else(|| queued_agent_prompt_preview(&user_message));
    Some(QueuedAgentPrompt {
        id,
        source_thread_id,
        authored_selector: entry.authored_selector,
        target_thread_id,
        user_message,
        preview,
        response_handling: Some(entry.response_handling),
    })
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
