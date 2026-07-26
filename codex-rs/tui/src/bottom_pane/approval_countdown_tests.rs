use super::super::tests::make_exec_request;
use super::super::tests::make_overlay;
use super::super::tests::make_permissions_request;
use super::super::tests::render_overlay_lines;
use super::*;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc::unbounded_channel;

#[test]
fn expired_command_approval_snapshot() {
    let (tx, mut rx) = unbounded_channel::<AppEvent>();
    let tx = AppEventSender::new(tx);
    let mut request = make_exec_request();
    {
        let ApprovalRequest::Exec(request) = &mut request else {
            panic!("expected exec approval request");
        };
        request.expires_at_ms = Some(0);
    }
    let mut view = make_overlay(request, tx, Features::with_defaults());

    assert_snapshot!(
        "approval_overlay_expired_command",
        render_overlay_lines(&view, /*width*/ 80)
    );
    assert!(view.pre_draw_tick(Instant::now()));
    assert!(rx.try_recv().is_err());
    assert!(view.is_complete());
    assert!(!view.terminal_title_requires_action());
}

#[test]
fn approval_expiration_formats_remaining_time() {
    assert_eq!(
        format_approval_expiration_remaining(Duration::from_millis(/*millis*/ 60_001)),
        "1m 01s"
    );
    assert_eq!(
        format_approval_expiration_remaining(Duration::from_secs(/*secs*/ 60)),
        "1m 00s"
    );
    assert_eq!(
        format_approval_expiration_remaining(Duration::from_millis(/*millis*/ 999)),
        "1s"
    );
    assert_eq!(
        approval_expiration_next_frame_delay(Duration::from_secs(/*secs*/ 60)),
        Some(Duration::from_secs(/*secs*/ 1))
    );
    assert_eq!(approval_expiration_next_frame_delay(Duration::ZERO), None);
}

#[test]
fn active_command_approval_countdown_snapshot() {
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let tx = AppEventSender::new(tx);
    let mut request = make_exec_request();
    let received_at = std::time::Instant::now();
    {
        let ApprovalRequest::Exec(request) = &mut request else {
            panic!("expected exec approval request");
        };
        request.received_at = received_at;
        request.started_at_ms = 0;
        request.expires_at_ms = Some(120_000);
    }
    let mut view = make_overlay(request, tx, Features::with_defaults());
    assert!(!view.pre_draw_tick(received_at + Duration::from_secs(/*secs*/ 60)));

    assert_snapshot!(
        "approval_overlay_active_countdown",
        render_overlay_lines(&view, /*width*/ 80)
    );
}

#[test]
fn expired_command_approval_advances_to_queued_request() {
    let now = Instant::now();
    let (tx, mut rx) = unbounded_channel::<AppEvent>();
    let tx = AppEventSender::new(tx);
    let mut expired = make_exec_request();
    let mut queued = make_exec_request();
    {
        let ApprovalRequest::Exec(request) = &mut expired else {
            panic!("expected exec approval request");
        };
        request.id = "expired".to_string();
        request.received_at = now;
        request.expires_at_ms = Some(0);
    }
    {
        let ApprovalRequest::Exec(request) = &mut queued else {
            panic!("expected exec approval request");
        };
        request.id = "queued".to_string();
        request.received_at = now - Duration::from_secs(/*secs*/ 30);
        request.started_at_ms = 0;
        request.expires_at_ms = Some(120_000);
    }
    let mut view = make_overlay(expired, tx, Features::with_defaults());
    view.enqueue_request(queued);

    assert!(view.pre_draw_tick(now));
    assert!(rx.try_recv().is_err());
    assert!(matches!(
        view.current_request.as_ref(),
        Some(ApprovalRequest::Exec(request)) if request.id == "queued"
    ));
    let remaining = view
        .current_deadline
        .expect("queued deadline")
        .saturating_duration_since(now);
    assert_eq!(remaining, Duration::from_secs(/*secs*/ 90));
    assert!(!view.is_complete());
}

#[test]
fn saturating_deadline_for_maximum_duration_remains_active() {
    let now = Instant::now();
    let deadline = saturating_instant_add(now, Duration::MAX);

    assert!(deadline > now);
}

#[test]
fn server_clock_offset_does_not_change_local_lifetime_and_input_does_not_restart_it() {
    let received_at = Instant::now();
    let mut deadlines = Vec::new();
    for started_at_ms in [-9_000_000, 9_000_000] {
        let (tx, mut rx) = unbounded_channel();
        let mut request = make_exec_request();
        let ApprovalRequest::Exec(exec) = &mut request else {
            unreachable!()
        };
        exec.started_at_ms = started_at_ms;
        exec.expires_at_ms = Some(started_at_ms + 60_000);
        exec.received_at = received_at;
        let mut view = make_overlay(request, AppEventSender::new(tx), Features::with_defaults());
        view.handle_key_event(KeyCode::Down.into());
        deadlines.push(view.current_deadline);
        assert!(view.pre_draw_tick(received_at + Duration::from_secs(/*secs*/ 60)));
        assert!(view.is_complete());
        assert!(rx.try_recv().is_err());
    }
    assert_eq!(
        deadlines,
        vec![Some(received_at + Duration::from_secs(/*secs*/ 60)); 2]
    );
}

#[test]
fn expired_shortcuts_enter_and_interrupt_emit_no_decision_or_fullscreen_request() {
    for code in [
        KeyCode::Enter,
        KeyCode::Char('y'),
        KeyCode::Esc,
        KeyCode::Char('a'),
    ] {
        let (tx, mut rx) = unbounded_channel();
        let mut request = make_exec_request();
        let ApprovalRequest::Exec(exec) = &mut request else {
            unreachable!()
        };
        exec.expires_at_ms = Some(0);
        let mut view = make_overlay(request, AppEventSender::new(tx), Features::with_defaults());
        view.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE));
        assert!(view.is_complete());
        assert!(rx.try_recv().is_err());
    }
    let (tx, mut rx) = unbounded_channel();
    let mut request = make_exec_request();
    let ApprovalRequest::Exec(exec) = &mut request else {
        unreachable!()
    };
    exec.expires_at_ms = Some(0);
    let mut view = make_overlay(request, AppEventSender::new(tx), Features::with_defaults());
    assert_eq!(view.on_ctrl_c(), CancellationEvent::Handled);
    assert!(view.is_complete());
    assert!(rx.try_recv().is_err());
}

#[test]
fn untimed_command_and_permissions_remain_interactive_after_time_passes() {
    for request in [make_exec_request(), make_permissions_request()] {
        let (tx, mut rx) = unbounded_channel();
        let mut view = make_overlay(request, AppEventSender::new(tx), Features::with_defaults());
        assert!(!view.pre_draw_tick(Instant::now() + Duration::from_secs(/*secs*/ 86_400)));
        assert!(!view.is_complete());
        assert!(view.terminal_title_requires_action());
        view.handle_key_event(KeyCode::Enter.into());
        assert!(
            std::iter::from_fn(|| rx.try_recv().ok())
                .any(|event| matches!(event, AppEvent::SubmitThreadOp { .. }))
        );
    }
}

#[test]
fn expired_escape_through_bottom_pane_does_not_cancel_the_next_approval() {
    let (tx, mut rx) = unbounded_channel();
    let mut pane = crate::bottom_pane::tests::test_pane(AppEventSender::new(tx));
    let features = Features::with_defaults();
    let mut expired = make_exec_request();
    let ApprovalRequest::Exec(exec) = &mut expired else {
        unreachable!()
    };
    exec.expires_at_ms = Some(0);
    let mut fresh = make_exec_request();
    let ApprovalRequest::Exec(exec) = &mut fresh else {
        unreachable!()
    };
    exec.id = "fresh".into();
    pane.push_approval_request(expired, &features);
    pane.push_approval_request(fresh, &features);
    pane.handle_key_event(KeyCode::Esc.into());
    assert!(pane.has_active_modal());
    assert!(rx.try_recv().is_err());
    pane.handle_key_event(KeyCode::Enter.into());
    let decisions = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::SubmitThreadOp {
                op: Op::ExecApproval { id, decision, .. },
                ..
            } => Some((id, decision)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        decisions,
        vec![("fresh".into(), CommandExecutionApprovalDecision::Accept)]
    );
}
