use super::PendingResponse;
use crate::AppServerEvent;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

fn server_error() -> JSONRPCErrorError {
    JSONRPCErrorError {
        code: -32000,
        data: Some(json!({"reason": "test"})),
        message: "request failed".into(),
    }
}

#[test]
fn replies_follow_preexisting_events_and_correlated_boundary() {
    for result in [Ok(json!({"ok": true})), Err(server_error())] {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        events_tx
            .send(AppServerEvent::Lagged { skipped: 1 })
            .unwrap();
        let (response_tx, mut response_rx) = oneshot::channel();
        let request_id = RequestId::Integer(3);
        PendingResponse {
            response_tx,
            ordered_boundary: Some(request_id.clone()),
        }
        .respond(result.clone(), &events_tx)
        .unwrap();

        assert_eq!(response_rx.try_recv().unwrap().unwrap(), result);
        assert!(matches!(
            events_rx.try_recv().unwrap(),
            AppServerEvent::Lagged { skipped: 1 }
        ));
        assert!(matches!(
            events_rx.try_recv().unwrap(),
            AppServerEvent::RequestCompleted { request_id: observed }
                if observed == request_id
        ));
        assert!(events_rx.try_recv().is_err());
    }
}

#[test]
fn dropped_reply_waiter_still_emits_boundary() {
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let (response_tx, response_rx) = oneshot::channel();
    drop(response_rx);
    let request_id = RequestId::String("dropped".into());
    PendingResponse {
        response_tx,
        ordered_boundary: Some(request_id.clone()),
    }
    .respond(Ok(json!({"accepted": true})), &events_tx)
    .unwrap();

    assert!(matches!(
        events_rx.try_recv().unwrap(),
        AppServerEvent::RequestCompleted { request_id: observed }
            if observed == request_id
    ));
}

#[test]
fn lost_event_stream_drops_reply_instead_of_confirming_it() {
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    drop(events_rx);
    let (response_tx, mut response_rx) = oneshot::channel();
    assert!(
        PendingResponse {
            response_tx,
            ordered_boundary: Some(RequestId::String("closed".into())),
        }
        .respond(Ok(json!({"accepted": true})), &events_tx)
        .is_err()
    );
    assert!(matches!(
        response_rx.try_recv(),
        Err(oneshot::error::TryRecvError::Closed)
    ));
}

#[test]
fn ordinary_replies_do_not_depend_on_the_event_stream() {
    for result in [Ok(json!({"ok": true})), Err(server_error())] {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        drop(events_rx);
        let (response_tx, mut response_rx) = oneshot::channel();
        PendingResponse {
            response_tx,
            ordered_boundary: None,
        }
        .respond(result.clone(), &events_tx)
        .unwrap();

        assert_eq!(response_rx.try_recv().unwrap().unwrap(), result);
    }
}
