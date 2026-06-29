use super::*;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc::unbounded_channel;

#[test]
fn countdown_rounds_up_expires_and_preserves_phase_elapsed() {
    let now = Instant::now();
    let (tx, _rx) = unbounded_channel();
    let mut row = StatusIndicatorWidget::new(
        AppEventSender::new(tx),
        FrameRequester::test_dummy(),
        /*animations_enabled*/ false,
        Default::default(),
    );
    row.update_header("Waiting for background terminal".into());
    let mut timer = StatusTimer::default();
    timer.pause_at(now);
    timer.display_started_at = Some(now - Duration::from_secs(/*secs*/ 7));
    timer.countdown_deadline = Some(now + Duration::from_millis(/*millis*/ 59_001));
    let render = |at| {
        StatusIndicator {
            row: &row,
            timer: &timer,
        }
        .lines_at(/*width*/ 100, at)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
    };
    insta::assert_snapshot!(render(now), @"Waiting for background terminal (1m 00s left • esc to interrupt)");
    insta::assert_snapshot!(render(now + Duration::from_secs(/*secs*/ 59)), @"Waiting for background terminal (1s left • esc to interrupt)");
    insta::assert_snapshot!(render(now + Duration::from_secs(/*secs*/ 60)), @"Waiting for background terminal (1m 07s • esc to interrupt)");
    assert_eq!(
        timer.countdown_remaining_seconds_at(now + Duration::from_secs(/*secs*/ 60)),
        None
    );
}

#[test]
fn reduced_motion_countdown_keeps_remapped_interrupt_and_survives_row_recreation() {
    let now = Instant::now();
    let timer = StatusTimer {
        countdown_deadline: Some(now + Duration::from_secs(/*secs*/ 2)),
        ..StatusTimer::default()
    };
    let (tx, _rx) = unbounded_channel();
    let mut rendered = Vec::new();
    for header in ["Waiting for Ada", "Waiting for agents"] {
        let mut row = StatusIndicatorWidget::new(
            AppEventSender::new(tx.clone()),
            FrameRequester::test_dummy(),
            /*animations_enabled*/ false,
            Default::default(),
        );
        row.update_header(header.into());
        row.set_interrupt_binding(Some(key_hint::plain(KeyCode::F(2)).into()));
        let lines = StatusIndicator {
            row: &row,
            timer: &timer,
        }
        .lines_at(/*width*/ 100, now);
        assert_eq!(
            lines[0].to_string(),
            format!("{header} (2s left • f2 to interrupt)")
        );
        rendered.push(lines[0].to_string());
        rendered.push(
            StatusIndicator {
                row: &row,
                timer: &timer,
            }
            .lines_at(/*width*/ 20, now)[0]
                .to_string(),
        );
    }
    insta::assert_snapshot!(rendered.join("\n"), @"
    Waiting for Ada (2s left • f2 to interrupt)
    Waiting for Ada (2s…
    Waiting for agents (2s left • f2 to interrupt)
    Waiting for agents …
    ");
}
