//! Transcript cells for interruptible clock sleeps.

use super::HistoryCell;
use super::plain_lines;
use codex_app_server_protocol::SleepItem;
use ratatui::prelude::*;
use ratatui::style::Stylize;

#[derive(Debug)]
pub(crate) struct SleepCell {
    item: SleepItem,
    turn_id: String,
    state: SleepCellState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SleepCellState {
    Active,
    Compact,
}

impl SleepCell {
    fn new_active(item: SleepItem, turn_id: String) -> Self {
        Self {
            item,
            turn_id,
            state: SleepCellState::Active,
        }
    }

    fn new_compact(item: SleepItem, turn_id: String) -> Self {
        Self {
            item,
            turn_id,
            state: SleepCellState::Compact,
        }
    }

    pub(crate) fn call_id(&self) -> &str {
        &self.item.id
    }

    pub(crate) fn item(&self) -> &SleepItem {
        &self.item
    }

    pub(crate) fn turn_id(&self) -> &str {
        &self.turn_id
    }

    fn is_active(&self) -> bool {
        self.state == SleepCellState::Active
    }

    fn label_parts(&self) -> (&'static str, String) {
        let duration = format_sleep_duration(self.item.duration_ms);
        if self.is_active() {
            ("Sleeping", duration)
        } else {
            ("Sleep", format!("requested {duration}"))
        }
    }
}

impl HistoryCell for SleepCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let (header, detail) = self.label_parts();
        if self.is_active() {
            vec![
                vec![
                    "•".cyan(),
                    " ".into(),
                    header.bold().cyan(),
                    " · ".dim(),
                    detail.cyan(),
                ]
                .into(),
            ]
        } else {
            vec![
                vec![
                    "•".dim(),
                    " ".into(),
                    header.bold(),
                    " · ".dim(),
                    detail.dim(),
                ]
                .into(),
            ]
        }
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let (header, detail) = self.label_parts();
        plain_lines(vec![Line::from(format!("{header} · {detail}"))])
    }
}

pub(crate) fn new_active_sleep_cell(item: SleepItem, turn_id: String) -> SleepCell {
    SleepCell::new_active(item, turn_id)
}

pub(crate) fn new_compact_sleep_cell(item: SleepItem, turn_id: String) -> SleepCell {
    SleepCell::new_compact(item, turn_id)
}

fn format_sleep_duration(duration_ms: u64) -> String {
    if duration_ms < 1_000 {
        return format!("{duration_ms}ms");
    }

    let seconds = duration_ms / 1_000;
    let milliseconds = duration_ms % 1_000;
    let display_seconds = seconds % 60;
    let fractional_seconds = if milliseconds == 0 {
        format!("{display_seconds:02}s")
    } else {
        let fraction = format!("{milliseconds:03}");
        let fraction = fraction.trim_end_matches('0');
        format!("{display_seconds:02}.{fraction}s")
    };

    if seconds >= 3_600 {
        format!(
            "{}h {:02}m {fractional_seconds}",
            seconds / 3_600,
            (seconds % 3_600) / 60
        )
    } else if seconds >= 60 {
        format!("{}m {fractional_seconds}", seconds / 60)
    } else if milliseconds == 0 {
        format!("{seconds}s")
    } else {
        let fraction = format!("{milliseconds:03}");
        let fraction = fraction.trim_end_matches('0');
        format!("{seconds}.{fraction}s")
    }
}
