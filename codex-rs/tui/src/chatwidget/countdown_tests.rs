use super::*;
use pretty_assertions::assert_eq;

#[test]
fn wall_estimate_conversion_is_checked_and_past_deadlines_expire() {
    let now = Instant::now();
    let wall_now = std::time::UNIX_EPOCH + Duration::from_millis(/*millis*/ 10_000);
    assert_eq!(
        deadline_at_ms_to_instant(/*deadline_at_ms*/ 10_001, wall_now, now),
        now.checked_add(Duration::from_millis(/*millis*/ 1))
    );
    assert_eq!(
        deadline_at_ms_to_instant(/*deadline_at_ms*/ 1, wall_now, now),
        Some(now)
    );
    assert_eq!(deadline_at_ms_to_instant(i64::MIN, wall_now, now), None);
    assert_eq!(
        deadline_at_ms_to_instant(
            /*deadline_at_ms*/ 1,
            std::time::UNIX_EPOCH - Duration::from_secs(/*secs*/ 1),
            now
        ),
        None
    );
}
