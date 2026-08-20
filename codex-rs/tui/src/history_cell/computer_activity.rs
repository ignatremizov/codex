//! Group adjacent computer calls while displaying their complete chronological details.
//!
//! CUA calls and intervening transcript-only reasoning share this cell. Visible history items
//! and turn boundaries end the group.
//! Normal history and the transcript share the same source-preserving renderer.

use super::*;

#[derive(Debug)]
pub(crate) struct ComputerActivityCell {
    pub(crate) group: ActivityGroup<McpToolCallCell>,
}

impl Default for ComputerActivityCell {
    fn default() -> Self {
        Self {
            group: ActivityGroup::new(Vec::new()),
        }
    }
}

impl ComputerActivityCell {
    fn detailed_hyperlink_lines(&self, width: u16, mode: HistoryRenderMode) -> Vec<HyperlinkLine> {
        let mut lines = Vec::new();
        for (index, call) in self.group.calls.iter().enumerate() {
            lines.extend(call.transcript_hyperlink_lines(width));
            lines.extend(self.group.details.lines_after(index + 1, width, mode));
        }
        lines
    }

    pub(crate) fn call_ids(&self) -> impl Iterator<Item = &str> {
        self.group.calls.iter().map(McpToolCallCell::call_id)
    }

    /// Prepend validated historical calls without recreating pending live calls or their clocks.
    pub(crate) fn prepend(&mut self, older: Self) {
        self.group.prepend(older.group);
    }

    pub(crate) fn start(&mut self, call: McpToolCallCell) {
        if !self
            .group
            .calls
            .iter()
            .any(|existing| existing.call_id == call.call_id)
        {
            self.group.calls.push(call);
        }
    }

    /// Completion-only replay follows the same path as a live start followed by completion.
    pub(crate) fn complete(
        &mut self,
        call: McpToolCallCell,
        duration: Duration,
        result: Result<codex_protocol::mcp::CallToolResult, String>,
    ) {
        let id = call.call_id.clone();
        self.start(call);
        if let Some(call) = self.group.calls.iter_mut().find(|call| call.call_id == id) {
            call.complete(duration, result);
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.group.calls.iter().any(|call| call.result.is_none())
    }

    pub(crate) fn mark_failed(&mut self) {
        for call in &mut self.group.calls {
            if call.result.is_none() {
                call.mark_failed();
            }
        }
    }
}

impl HistoryCell for ComputerActivityCell {
    fn append_reasoning(&mut self, cell: Box<dyn HistoryCell>) -> Result<(), Box<dyn HistoryCell>> {
        if self.group.calls.is_empty() {
            Err(cell)
        } else {
            self.group.push_detail(std::sync::Arc::from(cell));
            Ok(())
        }
    }

    fn compact_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        self.display_hyperlink_lines(width)
    }

    fn has_hidden_activity_details(&self, _width: u16) -> bool {
        false
    }

    fn display_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        if width == 0 {
            return Vec::new();
        }
        self.transcript_hyperlink_lines(width)
    }

    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        visible_lines(self.display_hyperlink_lines(width))
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        visible_lines(self.transcript_hyperlink_lines(width))
    }

    fn transcript_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        self.detailed_hyperlink_lines(width, HistoryRenderMode::Rich)
    }

    fn activity_ids(&self) -> Vec<String> {
        self.call_ids().map(|id| format!("mcp:{id}")).collect()
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(visible_lines(
            self.detailed_hyperlink_lines(u16::MAX, HistoryRenderMode::Raw),
        ))
    }

    fn transcript_animation_tick(&self) -> Option<u64> {
        self.group
            .calls
            .iter()
            .filter_map(HistoryCell::transcript_animation_tick)
            .max()
    }
}

#[cfg(test)]
#[path = "computer_activity_tests.rs"]
mod tests;
