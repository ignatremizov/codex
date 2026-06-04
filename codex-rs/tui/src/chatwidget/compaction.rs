//! Live compaction status. Its wall clock is separate from the turn's running time,
//! and only a matching live completion contributes a duration to the transcript.

use super::*;

pub(super) const COMPACTION_HEADER: &str = "Compacting context";
pub(super) const COMPACTION_DETAILS: &str = "Making room to continue.";

#[derive(Debug)]
pub(super) struct ActiveCompaction {
    pub(super) id: String,
    pub(super) turn_id: String,
    pub(super) started_at: Instant,
    pub(super) status_message: Option<String>,
}

impl ChatWidget {
    pub(super) fn on_context_compaction_completed(
        &mut self,
        id: &str,
        turn_id: &str,
        from_replay: bool,
        summary: Option<String>,
        message: Option<String>,
        decode_error: Option<String>,
    ) {
        let mut header = "Context compacted".to_string();
        if let Some(active) = self.status_state.compaction.as_ref()
            && active.id == id
            && active.turn_id == turn_id
        {
            if !from_replay {
                let elapsed = crate::status_indicator_widget::fmt_elapsed_compact(
                    active.started_at.elapsed().as_secs(),
                );
                header = format!("Context compacted · {elapsed}");
            }
            self.clear_context_compaction();
        }
        self.add_to_history(history_cell::new_compaction(
            header,
            summary,
            message,
            decode_error,
            self.local_settings.tui.show_compact_summary,
        ));
        self.request_redraw();
    }

    pub(super) fn on_context_compaction_started(
        &mut self,
        id: String,
        turn_id: String,
        elapsed: Duration,
    ) {
        if self
            .status_state
            .compaction
            .as_ref()
            .is_some_and(|active| active.id == id && active.turn_id == turn_id)
        {
            return;
        }
        self.flush_answer_stream_with_separator();
        let now = Instant::now();
        let started_at = now.checked_sub(elapsed).unwrap_or(now);
        self.status_state.compaction = Some(ActiveCompaction {
            id,
            turn_id,
            started_at,
            status_message: None,
        });
        self.bottom_pane.set_status_timer_origin(Some(started_at));
        self.bottom_pane.ensure_status_indicator();
        self.set_status_header(COMPACTION_HEADER.to_string());
    }

    pub(super) fn clear_context_compaction(&mut self) {
        if self.status_state.compaction.take().is_some() {
            self.bottom_pane
                .set_status_timer_origin(/*started_at*/ None);
            self.set_status_header("Working".to_string());
        }
    }
}
