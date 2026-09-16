use codex_core::TurnInputRequest;
use codex_history::RolloutItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_once;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acknowledged_shutdown_persists_interruption_and_releases_writer_lease() {
    let server = start_mock_server().await;
    mount_response_once(
        &server,
        sse_response(sse(vec![
            ev_response_created("shutdown-response"),
            ev_completed("shutdown-response"),
        ]))
        .set_delay(Duration::from_secs(60)),
    )
    .await;
    let test = test_codex()
        .build_with_auto_env(&server)
        .await
        .expect("start thread");
    let thread_id = test.session_configured.thread_id;
    assert!(
        test.thread_store
            .reserve_thread_writers(vec![thread_id])
            .await
            .is_err(),
        "live runtime holds its writer lease"
    );
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "persist this turn before stopping".to_string(),
            text_elements: Vec::new(),
        }]))
        .await
        .expect("start active turn");
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnStarted(_))
    })
    .await;
    let (first, second) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(
            test.codex.shutdown_durably_and_wait(),
            test.codex.shutdown_durably_and_wait()
        )
    })
    .await
    .expect("durable shutdown completes");
    first.expect("first waiter");
    second.expect("concurrent waiter");
    test.codex
        .shutdown_durably_and_wait()
        .await
        .expect("shutdown remains acknowledged after loop termination");
    assert!(test.codex.submit(Op::Interrupt).await.is_err());
    let _reservation = test
        .thread_store
        .reserve_thread_writers(vec![thread_id])
        .await
        .expect("shutdown acknowledgement releases the writer lease");
    let rollout = tokio::fs::read_to_string(test.codex.rollout_path().expect("rollout"))
        .await
        .expect("read durable history");
    let aborted_turns = rollout
        .lines()
        .map(|line| codex_rollout::parse_rollout_line(line).expect("parse history"))
        .filter(|line| matches!(line.item, RolloutItem::EventMsg(EventMsg::TurnAborted(_))))
        .count();
    assert_eq!(aborted_turns, 1);
}
