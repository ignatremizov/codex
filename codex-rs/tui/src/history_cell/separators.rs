//! Turn separators and runtime-metrics labels for transcript history.

use super::*;

#[derive(Debug, Clone, Copy)]
pub(crate) enum FinalMessageSeparatorTiming {
    ElapsedCheckpoint(u64),
    TurnCompleted(u64),
}

impl FinalMessageSeparatorTiming {
    fn label(self) -> String {
        match self {
            Self::ElapsedCheckpoint(elapsed_seconds) => format!(
                "{} elapsed",
                crate::status_indicator_widget::fmt_elapsed_compact(elapsed_seconds)
            ),
            Self::TurnCompleted(elapsed_seconds) => format!(
                "Worked for {}",
                crate::status_indicator_widget::fmt_elapsed_compact(elapsed_seconds)
            ),
        }
    }
}

#[derive(Debug)]
/// A visual divider between assistant phases or turns, optionally carrying elapsed timing.
///
/// Callers emit this only where the transcript already needs an assistant-phase or completed-turn
/// boundary, so ordinary conversational output does not gain extra divider rows.
pub struct FinalMessageSeparator {
    timing: Option<FinalMessageSeparatorTiming>,
    runtime_metrics: Option<RuntimeMetricsSummary>,
}
impl FinalMessageSeparator {
    /// Creates a separator; completed turns should prefer protocol duration when available.
    pub(crate) fn new(
        timing: Option<FinalMessageSeparatorTiming>,
        runtime_metrics: Option<RuntimeMetricsSummary>,
    ) -> Self {
        Self {
            timing,
            runtime_metrics,
        }
    }
}
impl HistoryCell for FinalMessageSeparator {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut label_parts = Vec::new();
        if let Some(timing) = self.timing {
            label_parts.push(timing.label());
        }
        if let Some(metrics_label) = self.runtime_metrics.and_then(runtime_metrics_label) {
            label_parts.push(metrics_label);
        }

        if label_parts.is_empty() {
            return vec![Line::from_iter(["─".repeat(width as usize).dim()])];
        }

        let label = format!("─ {} ─", label_parts.join(" • "));
        let (label, _suffix, label_width) = take_prefix_by_width(&label, width as usize);
        vec![
            Line::from_iter([
                label,
                "─".repeat((width as usize).saturating_sub(label_width)),
            ])
            .dim(),
        ]
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut label_parts = Vec::new();
        if let Some(timing) = self.timing {
            label_parts.push(timing.label());
        }
        if let Some(metrics_label) = self.runtime_metrics.and_then(runtime_metrics_label) {
            label_parts.push(metrics_label);
        }
        if label_parts.is_empty() {
            Vec::new()
        } else {
            vec![Line::from(label_parts.join(" • "))]
        }
    }
}

pub(crate) fn runtime_metrics_label(summary: RuntimeMetricsSummary) -> Option<String> {
    let mut parts = Vec::new();
    if summary.tool_calls.count > 0 {
        let duration = format_duration_ms(summary.tool_calls.duration_ms);
        let calls = pluralize(summary.tool_calls.count, "call", "calls");
        parts.push(format!(
            "Local tools: {} {calls} ({duration})",
            summary.tool_calls.count
        ));
    }
    if summary.api_calls.count > 0 {
        let duration = format_duration_ms(summary.api_calls.duration_ms);
        let calls = pluralize(summary.api_calls.count, "call", "calls");
        parts.push(format!(
            "Inference: {} {calls} ({duration})",
            summary.api_calls.count
        ));
    }
    if summary.websocket_calls.count > 0 {
        let duration = format_duration_ms(summary.websocket_calls.duration_ms);
        parts.push(format!(
            "WebSocket: {} events send ({duration})",
            summary.websocket_calls.count
        ));
    }
    if summary.streaming_events.count > 0 {
        let duration = format_duration_ms(summary.streaming_events.duration_ms);
        let stream_label = pluralize(summary.streaming_events.count, "Stream", "Streams");
        let events = pluralize(summary.streaming_events.count, "event", "events");
        parts.push(format!(
            "{stream_label}: {} {events} ({duration})",
            summary.streaming_events.count
        ));
    }
    if summary.websocket_events.count > 0 {
        let duration = format_duration_ms(summary.websocket_events.duration_ms);
        parts.push(format!(
            "{} events received ({duration})",
            summary.websocket_events.count
        ));
    }
    if summary.responses_api_overhead_ms > 0 {
        let duration = format_duration_ms(summary.responses_api_overhead_ms);
        parts.push(format!("Responses API overhead: {duration}"));
    }
    if summary.responses_api_inference_time_ms > 0 {
        let duration = format_duration_ms(summary.responses_api_inference_time_ms);
        parts.push(format!("Responses API inference: {duration}"));
    }
    if summary.responses_api_engine_iapi_ttft_ms > 0
        || summary.responses_api_engine_service_ttft_ms > 0
    {
        let mut ttft_parts = Vec::new();
        if summary.responses_api_engine_iapi_ttft_ms > 0 {
            let duration = format_duration_ms(summary.responses_api_engine_iapi_ttft_ms);
            ttft_parts.push(format!("{duration} (iapi)"));
        }
        if summary.responses_api_engine_service_ttft_ms > 0 {
            let duration = format_duration_ms(summary.responses_api_engine_service_ttft_ms);
            ttft_parts.push(format!("{duration} (service)"));
        }
        parts.push(format!("TTFT: {}", ttft_parts.join(" ")));
    }
    if summary.responses_api_engine_iapi_tbt_ms > 0.0
        || summary.responses_api_engine_service_tbt_ms > 0.0
    {
        let mut tbt_parts = Vec::new();
        if summary.responses_api_engine_iapi_tbt_ms > 0.0 {
            let duration =
                format_duration_ms(summary.responses_api_engine_iapi_tbt_ms.round() as u64);
            tbt_parts.push(format!("{duration} (iapi)"));
        }
        if summary.responses_api_engine_service_tbt_ms > 0.0 {
            let duration =
                format_duration_ms(summary.responses_api_engine_service_tbt_ms.round() as u64);
            tbt_parts.push(format!("{duration} (service)"));
        }
        parts.push(format!("TBT: {}", tbt_parts.join(" ")));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" • "))
    }
}

fn format_duration_ms(duration_ms: u64) -> String {
    if duration_ms >= 1_000 {
        let seconds = duration_ms as f64 / 1_000.0;
        format!("{seconds:.1}s")
    } else {
        format!("{duration_ms}ms")
    }
}

fn pluralize(count: u64, singular: &'static str, plural: &'static str) -> &'static str {
    if count == 1 { singular } else { plural }
}
