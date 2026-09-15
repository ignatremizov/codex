//! Transcript cells for interruptible clock sleeps.

use super::HistoryCell;
use super::plain_lines;
use codex_app_server_protocol::SleepItem;
use codex_app_server_protocol::SleepOutcome;
use ratatui::prelude::*;
use ratatui::style::Stylize;

#[derive(Debug)]
pub(crate) struct SleepCell {
    item: SleepItem,
}

impl SleepCell {
    fn detail(&self) -> String {
        let requested = format!("requested {}", format_sleep_duration(self.item.duration_ms));
        let (Some(outcome), Some(elapsed_ms)) = (self.item.outcome, self.item.elapsed_ms) else {
            return requested;
        };
        let outcome = match outcome {
            SleepOutcome::Completed => "completed",
            SleepOutcome::Interrupted => "interrupted",
            SleepOutcome::Error => "error",
        };
        format!(
            "{outcome} after {} · {requested}",
            format_sleep_duration(elapsed_ms)
        )
    }
}

impl HistoryCell for SleepCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let detail = self.detail();
        vec![
            vec![
                "•".dim(),
                " ".into(),
                "Sleep".bold(),
                " · ".dim(),
                detail.dim(),
            ]
            .into(),
        ]
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(vec![Line::from(format!("Sleep · {}", self.detail()))])
    }
}

pub(crate) fn new_compact_sleep_cell(item: SleepItem) -> SleepCell {
    SleepCell { item }
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
