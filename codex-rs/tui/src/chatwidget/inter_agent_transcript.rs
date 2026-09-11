//! Incoming communication is transcript content, not completion of the local assistant stream.

use super::*;

pub(super) fn is_inter_agent_message(item: &ThreadItem) -> bool {
    matches!(
        item,
        ThreadItem::AgentMessage {
            inter_agent_source: Some(_),
            ..
        }
    )
}

impl ChatWidget {
    pub(super) fn on_inter_agent_message(&mut self, item: ThreadItem, from_replay: bool) {
        let ThreadItem::AgentMessage {
            id,
            text,
            phase,
            inter_agent_source: Some(_),
            ..
        } = item
        else {
            return;
        };
        if let Some(cell) = multi_agents::background_commentary_history_cell_from_agent_message(
            &id,
            &text,
            phase.as_ref(),
            self.local_settings.tui.agent_response_preview_lines,
            |thread_id| self.collab_agent_metadata(thread_id),
        ) {
            if from_replay {
                self.on_collab_event(cell);
            } else {
                self.on_async_agent_notice(cell);
            }
            return;
        }
        let context = self.thread_id.and_then(|thread_id| {
            crate::inline_visualization::InlineVisualizationContext::from_config(
                &self.config,
                thread_id,
            )
        });
        // Do not interpret assistant directives or mutate local-answer/question state.
        let cell = history_cell::AgentMarkdownCell::new_with_inline_visualizations_and_phase(
            text,
            self.config.cwd.as_path(),
            context,
            phase,
        );
        if from_replay {
            self.on_collab_event(cell);
        } else {
            self.on_async_agent_notice(cell);
        }
    }
}
