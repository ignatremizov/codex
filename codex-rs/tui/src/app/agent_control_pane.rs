//! Read-only detail panel for the `/agent` overview.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use crossterm::event::KeyCode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use codex_utils_elapsed::format_duration;

use super::App;
use super::agent_control_summary::AgentControlSummary;
use super::agent_control_summary::SpawnedAgentSettings;
use super::agent_control_summary::agent_fork_mode_label;
use super::agent_control_summary::spawned_agent_settings;
use super::agent_navigation::AgentNavigationState;
use super::agent_observation_display::AgentResponseObservationBinding;
use super::agent_picker::AGENT_PICKER_VIEW_ID;
use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionListHeight;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::SideContentWidth;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;
use crate::multi_agents::agent_picker_status_dot_spans;
use crate::render::renderable::Renderable;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_lines;

#[derive(Clone, Debug, Default)]
pub(super) struct AgentControlPaneDetails {
    lines: Vec<Line<'static>>,
}

impl AgentControlPaneDetails {
    pub(super) fn new(lines: Vec<Line<'static>>) -> Self {
        Self { lines }
    }

    fn wrapped_lines(&self, width: u16) -> Vec<Line<'static>> {
        word_wrap_lines(self.lines.clone(), RtOptions::new(width.max(1) as usize))
    }
}

#[derive(Clone, Debug)]
pub(super) struct AgentControlPanePreview {
    selected: Arc<Mutex<AgentControlPaneDetails>>,
    revision: Arc<AtomicU64>,
}

impl AgentControlPanePreview {
    pub(super) fn new(selected: AgentControlPaneDetails) -> Self {
        Self {
            selected: Arc::new(Mutex::new(selected)),
            revision: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(super) fn select(&self, selected: AgentControlPaneDetails) {
        *self
            .selected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = selected;
        self.revision.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn renderable(&self) -> AgentControlPanePreviewRenderable {
        AgentControlPanePreviewRenderable {
            selected: Arc::clone(&self.selected),
            revision: Arc::clone(&self.revision),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentControlEnterAction {
    SwitchThread,
    InspectTranscript,
}

impl AgentControlEnterAction {
    fn event(self, thread_id: codex_protocol::ThreadId) -> AppEvent {
        match self {
            Self::SwitchThread => AppEvent::SelectAgentThread(thread_id),
            Self::InspectTranscript => AppEvent::InspectAgentTranscript(thread_id),
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::SwitchThread => "Enter opens this thread",
            Self::InspectTranscript => "Enter inspects this transcript",
        }
    }
}

impl App {
    pub(super) fn agent_picker_selection_view_params(
        &self,
        selected: Option<usize>,
    ) -> SelectionViewParams {
        let mut initial_selected_idx = selected;
        let displayed_thread_id = self.current_displayed_thread_id();
        let mut details = Vec::new();
        let mut spawn_settings = HashMap::<_, SpawnedAgentSettings>::new();
        for channel in self.thread_event_channels.values() {
            let Ok(store) = channel.store.try_lock() else {
                continue;
            };
            for (thread_id, settings) in spawned_agent_settings(&store) {
                let merged = spawn_settings.entry(thread_id).or_default();
                if merged.fork_mode.is_none() {
                    merged.fork_mode = settings.fork_mode;
                }
                if merged.model_settings.is_none() {
                    merged.model_settings = settings.model_settings;
                }
            }
        }
        let transcript_shortcut = self
            .keymap
            .primary_hint(crate::keymap::KeymapContext::Global, "open_transcript")
            .map(crate::key_hint::ShortcutHint::display_label);
        let transcript_action_id = crate::keymap::keymap_action_id("global", "open_transcript");
        let transcript_bindings = self.keymap.app.open_transcript.clone();
        let tab_binding = crate::key_hint::plain(KeyCode::Tab);
        let transcript_reserves_tab = transcript_bindings.contains(&tab_binding)
            || transcript_action_id.is_some_and(|action_id| {
                self.keymap.chords.bindings.iter().any(|binding| {
                    binding.action == action_id && binding.chord.prefix == tab_binding
                })
            });
        let mut max_agent_name_width = 0;
        let mut max_agent_description_width = 0;
        let agent_root_thread_id = self.agent_root_thread_id();
        let items: Vec<SelectionItem> = self
            .agent_navigation
            .ordered_threads()
            .into_iter()
            .enumerate()
            .map(|(idx, (thread_id, entry))| {
                if initial_selected_idx.is_none() && displayed_thread_id == Some(thread_id) {
                    initial_selected_idx = Some(idx);
                }
                let id = thread_id;
                let base_name = self
                    .agent_navigation
                    .display_name(thread_id, agent_root_thread_id);
                let agent_alias = self.agent_navigation.alias(thread_id);
                let agent_ref = agent_alias.map(|alias| alias.agent_ref);
                let is_transferred = agent_alias.is_some_and(|alias| {
                    alias.state == codex_app_server_protocol::AgentAliasState::Transferred
                });
                let is_external = agent_root_thread_id != Some(thread_id)
                    && agent_alias.is_none()
                    && self.agent_navigation.parent_thread_id(thread_id).is_none()
                    && !self.side_threads.contains_key(&thread_id)
                    && !self.thread_event_channels.contains_key(&thread_id);
                let enter_action = if entry.is_closed || is_external || is_transferred {
                    AgentControlEnterAction::InspectTranscript
                } else {
                    AgentControlEnterAction::SwitchThread
                };
                let response_observation = displayed_thread_id.and_then(|observer| {
                    self.agent_navigation
                        .response_observation(observer, thread_id)
                });
                let has_pending_approval =
                    self.thread_event_channels
                        .get(&thread_id)
                        .is_some_and(|channel| {
                            channel
                                .store
                                .try_lock()
                                .is_ok_and(|store| store.has_pending_thread_approvals())
                        });
                let runtime_summary = self
                    .thread_event_channels
                    .get(&thread_id)
                    .and_then(|channel| channel.store.try_lock().ok())
                    .map(|store| AgentControlSummary::from_store(&store))
                    .unwrap_or_default();
                let spawned_settings = spawn_settings.get(&thread_id);
                let spawn_model =
                    spawned_settings.and_then(|settings| settings.model_settings.as_ref());
                let model = runtime_summary
                    .model
                    .clone()
                    .or_else(|| spawn_model.and_then(|settings| settings.model.clone()));
                let reasoning_effort = runtime_summary.reasoning_effort.clone().or_else(|| {
                    spawn_model
                        .and_then(|settings| settings.reasoning_effort.as_ref())
                        .map(ToString::to_string)
                });
                let model_detail = match (model.as_deref(), reasoning_effort.as_deref()) {
                    (Some(model), Some(reasoning_effort)) => {
                        Some(("Model", format!("{model} {reasoning_effort}")))
                    }
                    (Some(model), None) => Some(("Model", model.to_string())),
                    (None, Some(reasoning_effort)) => {
                        Some(("Reasoning", reasoning_effort.to_string()))
                    }
                    (None, None) => None,
                };
                let mut state_labels = Vec::new();
                if let Some(observation) = response_observation {
                    state_labels.push(observation.compact_label());
                }
                if has_pending_approval {
                    state_labels.push("approval".to_string());
                }
                let name = if state_labels.is_empty() {
                    base_name.clone()
                } else {
                    format!("{base_name} ({})", state_labels.join(" · "))
                };
                let uuid = thread_id.to_string();
                let status = if is_transferred {
                    "transferred"
                } else if is_external {
                    "external"
                } else if entry.is_closed {
                    "closed"
                } else if entry.is_running {
                    "running"
                } else {
                    runtime_summary
                        .terminal_outcome
                        .map_or("idle", |outcome| outcome.label())
                };
                let queued = self
                    .queued_agent_prompts
                    .get(&thread_id)
                    .map_or(0, VecDeque::len);
                let mut selected_description = vec![uuid.clone(), status.to_string()];
                if queued > 0 {
                    selected_description.push(format!("{queued} queued"));
                }
                if has_pending_approval {
                    selected_description.push("approval pending".to_string());
                }
                let selected_description = selected_description.join(" · ");
                let description = format!("{uuid} · {status}");
                let status_span = match status {
                    "running" => "running".green(),
                    "errored" => "errored".red(),
                    "interrupted" => "interrupted".magenta(),
                    "closed" | "external" | "transferred" => status.dim(),
                    other => other.into(),
                };
                let mut status_line = vec![status_span];
                if let Some(running_for) = runtime_summary.running_for {
                    status_line.push(format!(" {}", format_duration(running_for)).into());
                }
                if let Some(agent_ref) = agent_ref {
                    status_line.push(format!(" · ref {agent_ref}").dim());
                }
                let mut detail_lines = vec![
                    base_name.bold().into(),
                    status_line.into(),
                    vec!["UUID: ".bold(), uuid.clone().dim()].into(),
                ];
                if let Some((label, description)) = model_detail.as_ref() {
                    detail_lines
                        .push(vec![format!("{label}: ").bold(), description.clone().into()].into());
                }
                if let Some(parent_thread_id) = self.agent_navigation.parent_thread_id(thread_id) {
                    detail_lines
                        .push(vec!["Parent: ".bold(), parent_thread_id.to_string().dim()].into());
                }
                if let Some(agent_path) = entry
                    .agent_path
                    .as_deref()
                    .map(str::trim)
                    .filter(|agent_path| !agent_path.is_empty())
                {
                    detail_lines.push(vec!["Path: ".bold(), agent_path.to_string().dim()].into());
                }
                let mut activity_lines = Vec::new();
                if let Some(task_preview) = runtime_summary.task_preview {
                    activity_lines.push(vec!["Task: ".bold(), task_preview.into()].into());
                }
                if let Some(response_preview) = runtime_summary.response_preview {
                    activity_lines
                        .push(vec!["Latest response: ".bold(), response_preview.into()].into());
                }
                if let Some(fork_mode) = spawned_settings.and_then(|settings| settings.fork_mode) {
                    activity_lines.push(
                        vec!["Fork: ".bold(), agent_fork_mode_label(fork_mode).into()].into(),
                    );
                }
                if !activity_lines.is_empty() {
                    detail_lines.push("".into());
                    detail_lines.extend(activity_lines);
                }
                detail_lines.push("".into());
                if let Some(observation) = response_observation {
                    let applies_to = match observation.binding {
                        AgentResponseObservationBinding::Bound => "current turn",
                        AgentResponseObservationBinding::NextTurn => "next turn",
                    };
                    detail_lines.extend([
                        vec![
                            "Response: ".bold(),
                            observation.final_response.label().into(),
                            format!(" · {applies_to}").dim(),
                        ]
                        .into(),
                        vec![
                            "Commentary: ".bold(),
                            if observation.commentary {
                                "first item".into()
                            } else {
                                "none".dim()
                            },
                        ]
                        .into(),
                    ]);
                } else {
                    detail_lines.push(vec!["Response: ".bold(), "none".dim()].into());
                }
                detail_lines.push(vec!["Queued: ".bold(), queued.to_string().into()].into());
                if let Some(queue) = self.queued_agent_prompts.get(&thread_id) {
                    for (index, prompt) in queue.iter().take(3).enumerate() {
                        detail_lines.push(
                            vec![
                                format!("  {}. ", index + 1).dim(),
                                prompt.preview().into(),
                                format!(" · {}", prompt.response_label()).dim(),
                            ]
                            .into(),
                        );
                    }
                    if queue.len() > 3 {
                        detail_lines.push(format!("  … {} more", queue.len() - 3).dim().into());
                    }
                }
                detail_lines.push(
                    vec![
                        "Children: ".bold(),
                        self.agent_navigation
                            .child_count(thread_id)
                            .to_string()
                            .into(),
                    ]
                    .into(),
                );
                if has_pending_approval {
                    detail_lines.push(vec!["Approval: ".bold(), "pending".magenta()].into());
                }
                detail_lines.extend(["".into(), enter_action.hint().dim().into()]);
                details.push(AgentControlPaneDetails::new(detail_lines));
                let mut name_prefix_spans: Vec<Span<'static>> = agent_ref
                    .map(|agent_ref| vec![format!("{agent_ref} ").into()])
                    .unwrap_or_default();
                let depth = self.agent_navigation.depth(thread_id);
                if depth > 0 {
                    name_prefix_spans
                        .push(format!("{}↳ ", "  ".repeat(depth.saturating_sub(1))).dim());
                }
                name_prefix_spans.extend(agent_picker_status_dot_spans(entry.is_closed));
                let name_width = 2
                    + name_prefix_spans
                        .iter()
                        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                        .sum::<usize>()
                    + UnicodeWidthStr::width(name.as_str());
                max_agent_name_width = max_agent_name_width.max(name_width);
                max_agent_description_width = max_agent_description_width.max(
                    UnicodeWidthStr::width(description.as_str())
                        .max(UnicodeWidthStr::width(selected_description.as_str())),
                );
                let search_value = [
                    agent_ref.map(|agent_ref| agent_ref.to_string()),
                    Some(name.clone()),
                    Some(uuid),
                    entry.agent_nickname.clone(),
                    entry.agent_role.clone(),
                    entry.agent_path.clone(),
                    model,
                    reasoning_effort,
                    has_pending_approval.then_some("approval".to_string()),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
                SelectionItem {
                    name,
                    name_style: if displayed_thread_id == Some(thread_id) {
                        ratatui::style::Style::default().bold()
                    } else {
                        ratatui::style::Style::default()
                    },
                    name_prefix_spans,
                    description: Some(description),
                    selected_description: Some(selected_description),
                    is_current: displayed_thread_id == Some(thread_id),
                    actions: vec![Box::new(move |tx| {
                        tx.send(enter_action.event(id));
                    })],
                    secondary_action: Some(Box::new(move |tx| {
                        tx.send(AppEvent::OpenAgentActions(id));
                    })),
                    global_shortcut_action: Some(Box::new(move |tx| {
                        tx.send(AppEvent::InspectAgentTranscript(id));
                    })),
                    dismiss_on_select: matches!(
                        enter_action,
                        AgentControlEnterAction::SwitchThread
                    ),
                    search_value: Some(search_value),
                    ..Default::default()
                }
            })
            .collect();
        let detail = initial_selected_idx
            .and_then(|selected| details.get(selected))
            .cloned()
            .or_else(|| details.first().cloned())
            .unwrap_or_default();
        let detail_preview = AgentControlPanePreview::new(detail);
        let selection_preview = detail_preview.clone();
        let requested_list_width = max_agent_name_width
            .saturating_add(2)
            .saturating_add(max_agent_description_width);
        let requested_list_width = u16::try_from(requested_list_width).unwrap_or(u16::MAX);
        let footer_note = match (transcript_shortcut, transcript_reserves_tab) {
            (None, _) => "Tab opens controls for the selected agent.".dim().into(),
            (Some(shortcut), true) => format!("{shortcut} inspects transcript.").dim().into(),
            (Some(shortcut), false) => {
                format!("{shortcut} inspects transcript · Tab opens controls.")
                    .dim()
                    .into()
            }
        };

        SelectionViewParams {
            view_id: Some(AGENT_PICKER_VIEW_ID),
            title: Some("Agents".to_string()),
            subtitle: Some(AgentNavigationState::picker_subtitle()),
            footer_note: Some(footer_note),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            is_searchable: true,
            list_height: SelectionListHeight::FillAvailable,
            search_placeholder: Some("Filter by ref, name, role, path, or UUID".to_string()),
            show_row_numbers: false,
            initial_selected_idx,
            show_current_item_suffix: false,
            side_content: Box::new(detail_preview.renderable()),
            side_content_width: SideContentWidth::RemainingAfterList(requested_list_width),
            side_content_min_width: 48,
            global_shortcut_bindings: transcript_bindings,
            global_shortcut_action_id: transcript_action_id,
            on_selection_changed: Some(Box::new(move |selected, _tx| {
                if let Some(detail) = details.get(selected) {
                    selection_preview.select(detail.clone());
                }
            })),
            ..Default::default()
        }
    }
}

pub(super) struct AgentControlPanePreviewRenderable {
    selected: Arc<Mutex<AgentControlPaneDetails>>,
    revision: Arc<AtomicU64>,
}

impl std::fmt::Debug for AgentControlPanePreviewRenderable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentControlPanePreviewRenderable")
            .finish_non_exhaustive()
    }
}

impl Renderable for AgentControlPanePreviewRenderable {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let selected = self
            .selected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Paragraph::new(selected.wrapped_lines(area.width)).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.selected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .wrapped_lines(width)
            .len()
            .try_into()
            .unwrap_or(u16::MAX)
    }

    fn layout_revision(&self) -> Option<u64> {
        Some(self.revision.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
#[path = "agent_control_pane_tests.rs"]
mod tests;
