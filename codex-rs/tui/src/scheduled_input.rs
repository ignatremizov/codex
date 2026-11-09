//! Client-side time resolution. Stored schedules contain one UTC instant.

use chrono::DateTime;
use chrono::Days;
use chrono::Duration;
use chrono::Local;
use chrono::LocalResult;
use chrono::NaiveTime;
use chrono::TimeZone;
use codex_app_server_protocol::QueuedSubmission;
use codex_app_server_protocol::ThreadItemPayload;
use codex_app_server_protocol::ThreadQueueResultResponse;
use codex_protocol::models::MessagePhase;

pub(crate) const SCHEDULE_USAGE: &str = "Usage: /schedule <HH:MM|ISO time> <prompt>, /schedule in <2h> <prompt>, /schedule list, /schedule cancel <id>, /schedule result <id>";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScheduleAction {
    Create { scheduled_at: i64, message: String },
    List,
    Cancel(String),
    Result(String),
}

pub fn resolve_schedule_time(value: &str) -> Result<i64, String> {
    resolve_at(value, Local::now())
}

fn resolve_at<Tz: TimeZone>(value: &str, now: DateTime<Tz>) -> Result<i64, String> {
    let value = value.trim();
    if let Ok(time) = DateTime::parse_from_rfc3339(value) {
        return Ok(time.timestamp());
    }
    if let Some(delay) = value.strip_prefix("in ") {
        let delay = delay.trim();
        let split = delay
            .find(|ch: char| !ch.is_ascii_digit())
            .ok_or("Use a delay such as 'in 30m' or 'in 2h'.")?;
        let amount: i64 = delay[..split]
            .parse()
            .map_err(|_| "Invalid schedule delay.")?;
        let unit = match &delay[split..] {
            "s" => 1,
            "m" => 60,
            "h" => 3600,
            "d" => 86400,
            _ => return Err("Use s, m, h or d for a schedule delay.".into()),
        };
        let duration = amount
            .checked_mul(unit)
            .and_then(Duration::try_seconds)
            .ok_or("Schedule delay is too large.")?;
        return now
            .checked_add_signed(duration)
            .map(|time| time.timestamp())
            .ok_or_else(|| "Schedule time is out of range.".into());
    }
    let time = NaiveTime::parse_from_str(value, "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(value, "%H:%M:%S"))
        .map_err(|_| "Use HH:MM, an ISO time with an offset, or 'in 2h'.")?;
    let mut date = now.date_naive();
    if time <= now.time() {
        date = date
            .checked_add_days(Days::new(1))
            .ok_or("Schedule date is out of range.")?;
    }
    match now.timezone().from_local_datetime(&date.and_time(time)) {
        LocalResult::Single(time) => Ok(time.timestamp()),
        LocalResult::Ambiguous(_, _) | LocalResult::None => {
            Err("That local time is ambiguous or does not exist; use an ISO time with an explicit offset.".into())
        }
    }
}

pub(crate) fn parse_schedule_action(args: &str) -> Result<ScheduleAction, String> {
    let args = args.trim();
    let (command, rest) = args
        .split_once(char::is_whitespace)
        .map_or((args, ""), |(command, rest)| (command, rest.trim()));
    let (time, message) = match command {
        "list" if rest.is_empty() => return Ok(ScheduleAction::List),
        "cancel" | "result" => {
            let id = rest;
            if id.is_empty() || id.chars().any(char::is_whitespace) {
                return Err(SCHEDULE_USAGE.into());
            }
            return Ok(if command == "result" {
                ScheduleAction::Result(id.into())
            } else {
                ScheduleAction::Cancel(id.into())
            });
        }
        "in" => {
            let (delay, message) = rest.split_once(char::is_whitespace).ok_or(SCHEDULE_USAGE)?;
            (format!("in {delay}"), message.trim())
        }
        _ => (command.to_string(), rest),
    };
    if message.is_empty() {
        return Err(SCHEDULE_USAGE.into());
    }
    Ok(ScheduleAction::Create {
        scheduled_at: resolve_schedule_time(&time)?,
        message: message.to_string(),
    })
}

pub fn format_schedule_time(timestamp: i64) -> String {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|time| time.format("%Y-%m-%d %H:%M:%S %:z").to_string())
        .unwrap_or_else(|| format!("Unix time {timestamp}"))
}

pub fn schedule_summary(item: &QueuedSubmission) -> String {
    let text = item
        .input
        .iter()
        .filter_map(|input| match input {
            codex_app_server_protocol::UserInput::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let due = item
        .scheduled_at
        .map(format_schedule_time)
        .unwrap_or_default();
    let error = item
        .dispatch_error
        .as_ref()
        .map(|error| format!(" · {error}"))
        .unwrap_or_default();
    format!("{} · {due} · {text}{error}", item.id)
}

pub fn scheduled_result_text(result: &ThreadQueueResultResponse) -> Option<&str> {
    result
        .turn
        .as_ref()?
        .items
        .iter()
        .rev()
        .find_map(|item| match &item.payload {
            ThreadItemPayload::AgentMessage {
                text,
                phase: None | Some(MessagePhase::FinalAnswer),
                inter_agent_source: None,
                attribution: None,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
}

#[cfg(test)]
#[path = "scheduled_input_tests.rs"]
mod tests;
