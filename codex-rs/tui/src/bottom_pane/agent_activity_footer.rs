//! Presentation-only summary of running agents in the current root's live state.
//! This row does not control the current thread's busy state or input availability.

use crate::live_wrap::take_prefix_by_width;
use crate::render::renderable::Renderable;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

#[derive(Default)]
pub(super) struct AgentActivityFooter {
    running: usize,
}

impl super::BottomPane {
    pub(crate) fn has_running_agents(&self) -> bool {
        self.agent_activity_footer.running > 0
    }

    pub(crate) fn set_running_agent_count(&mut self, running: usize) {
        if self.agent_activity_footer.running != running {
            self.agent_activity_footer.running = running;
            self.request_redraw();
        }
    }
}

impl Renderable for AgentActivityFooter {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if self.desired_height(area.width) == 0 || area.is_empty() {
            return;
        }
        let count = self.running;
        let plural = if count == 1 { "" } else { "s" };
        let message = format!("  {count} agent{plural} running · /agent to view");
        let (truncated, _, _) = take_prefix_by_width(&message, usize::from(area.width));
        Paragraph::new(Line::from(truncated.dim())).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        u16::from(self.running > 0 && width >= 4)
    }
}

#[cfg(test)]
#[path = "agent_activity_footer_tests.rs"]
mod tests;
