use super::PendingClientResponse;
use crate::in_process::InProcessServerEvent;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::Duration;
use tokio::time::timeout;

fn server_error() -> JSONRPCErrorError {
    JSONRPCErrorError {
        code: -32000,
        data: Some(json!({"reason": "test"})),
        message: "request failed".into(),
    }
}

#[tokio::test]
async fn replies_follow_preexisting_events_and_correlated_boundary() {
    for result in [Ok(json!({"ok": true})), Err(server_error())] {
        let (events_tx, mut events_rx) = mpsc::channel(2);
        events_tx
            .try_send(InProcessServerEvent::Lagged { skipped: 1 })
            .unwrap();
        let (response_tx, mut response_rx) = oneshot::channel();
        let request_id = RequestId::String("rollback-1".into());

        PendingClientResponse {
            response_tx,
            ordered_boundary: Some(request_id.clone()),
        }
        .respond(result.clone(), &events_tx)
        .await
        .unwrap();

        assert_eq!(response_rx.try_recv().unwrap(), result);
        assert!(matches!(
            events_rx.try_recv().unwrap(),
            InProcessServerEvent::Lagged { skipped: 1 }
        ));
        assert!(matches!(
            events_rx.try_recv().unwrap(),
            InProcessServerEvent::RequestCompleted { request_id: observed }
                if observed == request_id
        ));
        assert!(events_rx.try_recv().is_err());
    }
}

#[tokio::test]
async fn bounded_event_pressure_prevents_an_early_reply() {
    let (events_tx, mut events_rx) = mpsc::channel(1);
    events_tx
        .try_send(InProcessServerEvent::Lagged { skipped: 2 })
        .unwrap();
    let (response_tx, mut response_rx) = oneshot::channel();
    let request_id = RequestId::Integer(7);
    let pending = PendingClientResponse {
        response_tx,
        ordered_boundary: Some(request_id.clone()),
    };
    let respond = pending.respond(Ok(json!({"accepted": true})), &events_tx);
    tokio::pin!(respond);

    // Poll the actual producer against a full queue, rather than relying on task scheduling.
    assert!(futures::poll!(&mut respond).is_pending());
    assert_eq!(
        response_rx.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    );
    assert!(matches!(
        events_rx.try_recv().unwrap(),
        InProcessServerEvent::Lagged { skipped: 2 }
    ));
    timeout(Duration::from_secs(5), respond)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        response_rx.try_recv().unwrap(),
        Ok(json!({"accepted": true}))
    );
    assert!(matches!(
        events_rx.try_recv().unwrap(),
        InProcessServerEvent::RequestCompleted { request_id: observed }
            if observed == request_id
    ));
}

#[tokio::test]
async fn dropped_reply_waiter_still_enqueues_boundary() {
    let (events_tx, mut events_rx) = mpsc::channel(1);
    let (response_tx, response_rx) = oneshot::channel();
    drop(response_rx);
    let request_id = RequestId::String("dropped".into());
    PendingClientResponse {
        response_tx,
        ordered_boundary: Some(request_id.clone()),
    }
    .respond(Ok(json!({"accepted": true})), &events_tx)
    .await
    .unwrap();

    assert!(matches!(
        events_rx.try_recv().unwrap(),
        InProcessServerEvent::RequestCompleted { request_id: observed }
            if observed == request_id
    ));
}

#[tokio::test]
async fn lost_event_stream_drops_reply_instead_of_confirming_it() {
    let (events_tx, events_rx) = mpsc::channel(1);
    drop(events_rx);
    let (response_tx, mut response_rx) = oneshot::channel();
    let result = PendingClientResponse {
        response_tx,
        ordered_boundary: Some(RequestId::String("closed".into())),
    }
    .respond(Ok(json!({"accepted": true})), &events_tx)
    .await;

    assert!(result.is_err());
    assert_eq!(
        response_rx.try_recv(),
        Err(oneshot::error::TryRecvError::Closed)
    );
}

#[tokio::test]
async fn ordinary_replies_do_not_depend_on_the_event_stream() {
    for result in [Ok(json!({"ok": true})), Err(server_error())] {
        let (events_tx, events_rx) = mpsc::channel(1);
        drop(events_rx);
        let (response_tx, mut response_rx) = oneshot::channel();
        PendingClientResponse {
            response_tx,
            ordered_boundary: None,
        }
        .respond(result.clone(), &events_tx)
        .await
        .unwrap();

        assert_eq!(response_rx.try_recv().unwrap(), result);
    }
}
