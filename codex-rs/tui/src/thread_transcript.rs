//! Render persisted thread turns into history-cell building blocks.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::app_server_session::AppServerSession;
use crate::app_server_session::HistoryHydrationScope;
use crate::chatwidget::ChatWidget;
use crate::exec_cell::ActiveExecCall;
use crate::exec_cell::CommandOutput;
use crate::exec_cell::OutputPreviewLineLimits;
use crate::exec_cell::new_active_exec_command;
use crate::exec_command::split_command_string;
use crate::git_action_directives::parse_assistant_markdown;
use crate::history_cell::AgentMarkdownCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::PlainHistoryCell;
use crate::history_cell::PrefixedWrappedHistoryCell;
use crate::history_cell::ReasoningSummaryCell;
use crate::history_cell::UserHistoryCell;
use crate::history_cell::split_reasoning_summary_parts;
use crate::inline_visualization::InlineVisualizationContext;
use crate::legacy_core::config::Config;
use crate::multi_agents::AgentMetadata;
use crate::multi_agents::CollabAgentHistoryCell;
use crate::multi_agents::background_commentary_history_cell_from_agent_message;
use crate::multi_agents::background_completion_history_cell_from_agent_message;
use crate::multi_agents::parse_thread_id;
use crate::multi_agents::sub_agent_activity_summary;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadItem;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::AbsolutePathBuf;
use ratatui::style::Stylize as _;
use ratatui::text::Line;

pub(crate) type TranscriptCells = Vec<Arc<dyn HistoryCell>>;
pub(crate) type CollabAgentMetadataMap = HashMap<ThreadId, AgentMetadata>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RawReasoningVisibility {
    Hidden,
    Visible,
}

pub(crate) async fn load_session_transcript(
    app_server: &mut AppServerSession,
    thread_id: ThreadId,
    raw_reasoning_visibility: RawReasoningVisibility,
    config: Option<&Config>,
) -> std::io::Result<TranscriptCells> {
    let mut thread = app_server
        .thread_read(thread_id, /*include_turns*/ false)
        .await
        .map_err(std::io::Error::other)?;
    app_server
        .hydrate_initial_thread_history(
            &mut thread,
            /*turn_cursor*/ None,
            /*item_cursor*/ None,
            /*config*/ None,
            HistoryHydrationScope::Complete,
        )
        .await
        .map_err(std::io::Error::other)?;
    Ok(thread_to_transcript_cells(
        thread,
        raw_reasoning_visibility,
        config,
    ))
}

pub(crate) fn thread_to_transcript_cells(
    thread: Thread,
    raw_reasoning_visibility: RawReasoningVisibility,
    config: Option<&Config>,
) -> TranscriptCells {
    let cwd = thread.cwd;
    let thread_id = ThreadId::from_string(&thread.id).ok();
    let items = thread
        .turns
        .into_iter()
        .flat_map(|turn| {
            let turn_id = turn.id;
            turn.items
                .into_iter()
                .map(move |item| (Some(turn_id.clone()), item))
        })
        .collect::<Vec<_>>();
    let metadata = collab_agent_metadata_from_items(items.iter().map(|(_, item)| item));
    let mut cells = thread_items_with_sources_to_transcript_cells(
        thread_id,
        &cwd,
        items,
        raw_reasoning_visibility,
        config,
        &metadata,
    );
    if cells.is_empty() {
        cells.push(Arc::new(PlainHistoryCell::new(vec![
            "No transcript content available".italic().dim().into(),
        ])));
    }
    cells
}

pub(crate) fn collab_agent_metadata_from_items<'a>(
    items: impl IntoIterator<Item = &'a ThreadItem>,
) -> CollabAgentMetadataMap {
    let mut metadata = CollabAgentMetadataMap::new();
    extend_collab_agent_metadata(&mut metadata, items);
    metadata
}

pub(crate) fn thread_items_to_transcript_cells_with_metadata(
    thread_id: Option<ThreadId>,
    cwd: &AbsolutePathBuf,
    items: impl IntoIterator<Item = ThreadItem>,
    raw_reasoning_visibility: RawReasoningVisibility,
    config: Option<&Config>,
    known_collab_agent_metadata: &CollabAgentMetadataMap,
) -> TranscriptCells {
    thread_items_with_sources_to_transcript_cells(
        thread_id,
        cwd,
        items.into_iter().map(|item| (None, item)),
        raw_reasoning_visibility,
        config,
        known_collab_agent_metadata,
    )
}

pub(crate) fn thread_items_with_sources_to_transcript_cells(
    thread_id: Option<ThreadId>,
    cwd: &AbsolutePathBuf,
    items: impl IntoIterator<Item = (Option<String>, ThreadItem)>,
    raw_reasoning_visibility: RawReasoningVisibility,
    config: Option<&Config>,
    known_collab_agent_metadata: &CollabAgentMetadataMap,
) -> TranscriptCells {
    let inline_visualization_context = config.and_then(|config| {
        thread_id.and_then(|thread_id| InlineVisualizationContext::from_config(config, thread_id))
    });
    let mut cells: TranscriptCells = Vec::new();
    for (turn_id, item) in items {
        match item {
            ThreadItem::UserMessage { id, content, .. } => {
                let display = ChatWidget::user_message_display_from_inputs(&content);
                cells.push(Arc::new(UserHistoryCell {
                    message: display.message,
                    text_elements: display.text_elements,
                    local_image_paths: display.local_images,
                    remote_image_urls: display.remote_image_urls,
                    source: turn_id.map(|turn_id| crate::history_cell::UserMessageSource {
                        item_id: id,
                        turn_id,
                    }),
                }));
            }
            ThreadItem::AgentMessage {
                id, text, phase, ..
            } => {
                let collab_cell = background_completion_history_cell_from_agent_message(
                    &id,
                    &text,
                    phase.as_ref(),
                    /*agent_response_preview_lines*/ 0,
                    |thread_id| {
                        known_collab_agent_metadata
                            .get(&thread_id)
                            .cloned()
                            .unwrap_or_default()
                    },
                )
                .or_else(|| {
                    background_commentary_history_cell_from_agent_message(
                        &id,
                        &text,
                        phase.as_ref(),
                        /*agent_response_preview_lines*/ 0,
                        |thread_id| {
                            known_collab_agent_metadata
                                .get(&thread_id)
                                .cloned()
                                .unwrap_or_default()
                        },
                    )
                });
                if let Some(cell) = collab_cell {
                    cells.push(Arc::new(cell));
                    continue;
                }
                let parsed = parse_assistant_markdown(&text, cwd.as_path());
                if !parsed.visible_markdown.trim().is_empty() {
                    cells.push(Arc::new(AgentMarkdownCell::new_with_inline_visualizations(
                        parsed.visible_markdown,
                        cwd.as_path(),
                        inline_visualization_context.clone(),
                    )));
                }
            }
            ThreadItem::FunctionCallOutput {
                name,
                namespace,
                output,
                ..
            } => {
                if let Some((source_thread_id, prompt)) =
                    crate::dynamic_tools::parse_delegated_tool_output(
                        &name,
                        namespace.as_deref(),
                        &output,
                    )
                {
                    cells.push(Arc::new(PrefixedWrappedHistoryCell::new(
                        format!("Sent by Codex from task {source_thread_id}\n{prompt}"),
                        "• ".dim(),
                        "  ",
                    )));
                }
            }
            ThreadItem::Plan { text, .. } => {
                if !text.trim().is_empty() {
                    cells.push(Arc::new(crate::history_cell::new_proposed_plan(
                        text,
                        cwd.as_path(),
                    )));
                }
            }
            ThreadItem::Reasoning {
                summary, content, ..
            } => {
                let (header, text) =
                    if matches!(raw_reasoning_visibility, RawReasoningVisibility::Visible)
                        && !content.is_empty()
                    {
                        ("Reasoning".to_string(), content.join("\n\n"))
                    } else {
                        split_reasoning_summary_parts(&summary)
                    };
                if !text.trim().is_empty() {
                    cells.push(Arc::new(ReasoningSummaryCell::new(
                        header,
                        text,
                        cwd.as_path(),
                        /*transcript_only*/ false,
                    )));
                }
            }
            item @ ThreadItem::UserAgentControl { .. } => {
                if let Some(cell) = crate::history_cell::new_user_agent_control(item) {
                    cells.push(Arc::new(cell));
                }
            }
            other => {
                if let Some(cell) = fallback_transcript_cell(&other, config) {
                    cells.push(cell);
                }
            }
        }
    }
    cells
}

pub(crate) fn refresh_collab_agent_metadata(
    cells: &mut [Arc<dyn HistoryCell>],
    known_collab_agent_metadata: &CollabAgentMetadataMap,
) -> bool {
    let mut refreshed = false;
    for cell in cells {
        let Some(collab_cell) = cell.as_any().downcast_ref::<CollabAgentHistoryCell>() else {
            continue;
        };
        let Some(updated) = collab_cell.with_refreshed_agent_metadata(|thread_id| {
            known_collab_agent_metadata.get(&thread_id).cloned()
        }) else {
            continue;
        };
        *cell = Arc::new(updated);
        refreshed = true;
    }
    refreshed
}

fn extend_collab_agent_metadata<'a>(
    metadata: &mut CollabAgentMetadataMap,
    items: impl IntoIterator<Item = &'a ThreadItem>,
) {
    for item in items {
        match item {
            ThreadItem::CollabAgentToolCall {
                receiver_thread_ids,
                receiver_agents,
                ..
            } => {
                for receiver in receiver_agents {
                    let Some(thread_id) = parse_thread_id(&receiver.thread_id) else {
                        continue;
                    };
                    let metadata = metadata.entry(thread_id).or_default();
                    if receiver.agent_nickname.is_some() {
                        metadata.agent_nickname = receiver.agent_nickname.clone();
                    }
                    if receiver.agent_role.is_some() {
                        metadata.agent_role = receiver.agent_role.clone();
                    }
                }
                if let Some(spawn_request) = crate::multi_agents::spawn_request_summary(item) {
                    for receiver_thread_id in receiver_thread_ids {
                        let Some(thread_id) = parse_thread_id(receiver_thread_id) else {
                            continue;
                        };
                        metadata.entry(thread_id).or_default().spawn_request =
                            Some(spawn_request.clone());
                    }
                }
            }
            ThreadItem::UserAgentControl {
                target_thread_id: Some(target_thread_id),
                nickname,
                role,
                ..
            } => {
                let Some(thread_id) = parse_thread_id(target_thread_id) else {
                    continue;
                };
                let metadata = metadata.entry(thread_id).or_default();
                if nickname.is_some() {
                    metadata.agent_nickname = nickname.clone();
                }
                if role.is_some() {
                    metadata.agent_role = role.clone();
                }
                if let Some(spawn_request) = crate::multi_agents::spawn_request_summary(item) {
                    metadata.spawn_request = Some(spawn_request);
                }
            }
            _ => {}
        }
    }
}

fn fallback_transcript_cell(
    item: &ThreadItem,
    config: Option<&Config>,
) -> Option<Arc<dyn HistoryCell>> {
    let lines = match item {
        ThreadItem::HookPrompt { fragments, .. } => fragments
            .iter()
            .map(|fragment| {
                vec![
                    "hook prompt: ".dim(),
                    fragment.text.trim().to_string().into(),
                ]
                .into()
            })
            .collect::<Vec<_>>(),
        ThreadItem::CommandExecution {
            command,
            status,
            user_shell_response_handling,
            source,
            command_actions,
            aggregated_output,
            exit_code,
            duration_ms,
            ..
        } => {
            let parsed = command_actions
                .iter()
                .cloned()
                .map(codex_app_server_protocol::CommandAction::into_core)
                .collect();
            let output = if *source
                == codex_app_server_protocol::CommandExecutionSource::UnifiedExecInteraction
            {
                String::new()
            } else {
                aggregated_output.clone().unwrap_or_default()
            };
            let exit_code =
                if *status == codex_app_server_protocol::CommandExecutionStatus::Completed {
                    exit_code.unwrap_or_default()
                } else {
                    exit_code.filter(|code| *code != 0).unwrap_or(1)
                };
            let limits = config.map_or(
                OutputPreviewLineLimits {
                    command: codex_config::types::DEFAULT_TUI_COMMAND_OUTPUT_PREVIEW_LINES,
                    user_shell: codex_config::types::DEFAULT_TUI_USER_SHELL_OUTPUT_PREVIEW_LINES,
                },
                |config| OutputPreviewLineLimits {
                    command: config.tui_command_output_preview_lines,
                    user_shell: config.tui_user_shell_output_preview_lines,
                },
            );
            let mut cell = new_active_exec_command(
                ActiveExecCall {
                    call_id: id.clone(),
                    command: split_command_string(command),
                    parsed,
                    source: *source,
                    user_shell_response_handling: *user_shell_response_handling,
                    interaction_input: None,
                },
                config.is_some_and(|config| config.animations),
                limits,
            );
            if *status != codex_app_server_protocol::CommandExecutionStatus::InProgress {
                let completed = cell.complete_call(
                    id,
                    CommandOutput::new(exit_code, output),
                    Duration::from_millis(duration_ms.unwrap_or_default().max(0) as u64),
                );
                debug_assert!(completed, "hydrated exec cell should contain {id}");
            }
            return Some(Arc::new(cell));
        }
        ThreadItem::FileChange {
            changes, status, ..
        } => vec![
            format!("file changes: {status:?} · {} changes", changes.len())
                .dim()
                .into(),
        ],
        ThreadItem::McpToolCall {
            server,
            tool,
            status,
            ..
        } => vec![
            format!("mcp tool: {server}/{tool} · {status:?}")
                .dim()
                .into(),
        ],
        ThreadItem::DynamicToolCall {
            namespace,
            tool,
            status,
            ..
        } => {
            let name = namespace
                .as_ref()
                .map(|namespace| format!("{namespace}/{tool}"))
                .unwrap_or_else(|| tool.clone());
            vec![format!("tool: {name} · {status:?}").dim().into()]
        }
        ThreadItem::CollabAgentToolCall {
            tool,
            status,
            observe_commentary,
            wake_on_completion,
            ..
        } => {
            let commentary = match observe_commentary {
                Some(true) => " · receive commentary",
                Some(false) => " · no commentary",
                None => "",
            };
            let wake = match wake_on_completion {
                Some(true) => " · wake on completion",
                Some(false) => " · no wake on completion",
                None => "",
            };
            vec![
                format!("agent tool: {tool:?} · {status:?}{commentary}{wake}")
                    .dim()
                    .into(),
            ]
        }
        ThreadItem::SubAgentActivity {
            kind, agent_path, ..
        } => {
            vec![sub_agent_activity_summary(*kind, agent_path).dim().into()]
        }
        ThreadItem::WebSearch(item) => {
            vec![vec!["web search: ".dim(), item.query.clone().into()].into()]
        }
        ThreadItem::ImageView { path, .. } => {
            let path = path.render_for_ui();
            vec![format!("image: {path}").dim().into()]
        }
        ThreadItem::ImageGeneration(item) => {
            let saved = item
                .saved_path
                .as_ref()
                .map(|path| format!(" · {}", path.as_path().display()))
                .unwrap_or_default();
            vec![
                format!("image generation: {}{saved}", item.status)
                    .dim()
                    .into(),
            ]
        }
        ThreadItem::EnteredReviewMode { review, .. } => {
            vec![vec!["review started: ".dim(), review.clone().into()].into()]
        }
        ThreadItem::ExitedReviewMode { review, .. } => {
            vec![vec!["review finished: ".dim(), review.clone().into()].into()]
        }
        ThreadItem::ContextCompaction {
            decode_error,
            available_skills,
            ..
        } => {
            let mut lines = if let Some(error) = decode_error {
                vec![
                    vec![
                        "context compacted · prompt decoding failed: ".dim(),
                        error.clone().red(),
                    ]
                    .into(),
                ]
            } else {
                vec!["context compacted".dim().into()]
            };
            if !available_skills.is_empty() {
                lines.push(
                    vec![
                        "available skills after compaction: ".dim(),
                        available_skills.join(", ").into(),
                    ]
                    .into(),
                );
            }
            lines
        }
        ThreadItem::UserMessage { .. }
        | ThreadItem::AgentMessage { .. }
        | ThreadItem::FunctionCallOutput { .. }
        | ThreadItem::Plan { .. }
        | ThreadItem::Reasoning { .. }
        | ThreadItem::UserAgentControl { .. }
        | ThreadItem::Sleep(_) => return None,
    };
    (!lines.is_empty()).then(|| Arc::new(PlainHistoryCell::new(lines)) as Arc<dyn HistoryCell>)
}

#[cfg(test)]
#[path = "thread_transcript_tests.rs"]
mod tests;
