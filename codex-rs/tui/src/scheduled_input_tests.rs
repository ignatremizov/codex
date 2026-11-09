use super::ScheduleAction;
use super::parse_schedule_action;
use super::resolve_at;
use chrono::DateTime;
use pretty_assertions::assert_eq;

#[test]
fn resolves_offsets_delays_and_next_local_occurrence() -> Result<(), Box<dyn std::error::Error>> {
    let now = DateTime::parse_from_rfc3339("2026-11-09T17:30:00+02:00")?;
    let tomorrow = DateTime::parse_from_rfc3339("2026-11-10T17:30:00+02:00")?;
    assert_eq!(
        [
            resolve_at("17:30", now)?,
            resolve_at("in 2h", now)?,
            resolve_at("2026-11-08T12:00:00Z", now)?,
        ],
        [
            tomorrow.timestamp(),
            now.timestamp() + 7200,
            DateTime::parse_from_rfc3339("2026-11-08T12:00:00Z")?.timestamp(),
        ]
    );
    for invalid in [
        "in -2h",
        "in 2",
        "in 9223372036854775807d",
        "25:00",
        "tomorrow",
    ] {
        assert!(resolve_at(invalid, now).is_err(), "{invalid}");
    }
    Ok(())
}

#[test]
fn command_parser_preserves_prompt_and_validates_management_arguments() -> Result<(), String> {
    let at = "2026-11-10T17:30:00+02:00";
    assert_eq!(
        parse_schedule_action(&format!("{at} review this\nand that"))?,
        ScheduleAction::Create {
            scheduled_at: DateTime::parse_from_rfc3339(at)
                .map_err(|error| error.to_string())?
                .timestamp(),
            message: "review this\nand that".into(),
        }
    );
    assert_eq!(
        [
            parse_schedule_action("list")?,
            parse_schedule_action("cancel input-1")?,
            parse_schedule_action("result input-1")?,
        ],
        [
            ScheduleAction::List,
            ScheduleAction::Cancel("input-1".into()),
            ScheduleAction::Result("input-1".into())
        ]
    );
    for invalid in ["", "in 2h", "17:30", "cancel", "result one two"] {
        assert!(parse_schedule_action(invalid).is_err(), "{invalid}");
    }
    Ok(())
}
