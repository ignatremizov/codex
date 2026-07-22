use super::*;
use codex_app_server_protocol::JSONRPCErrorError;

#[test]
fn committed_and_unknown_errors_are_not_rejections() {
    for (key, committed) in [
        ("threadRollbackCommitted", true),
        ("threadRollbackRefreshRequired", false),
    ] {
        let source: JSONRPCErrorError = serde_json::from_value(serde_json::json!({
            "code": -32603, "message": "refresh required", "data": {key: true}
        }))
        .expect("error");
        let outcome = rollback_outcome(Err(TypedRequestError::Server {
            method: "thread/rollback".into(),
            source,
        }));
        assert!(matches!(
            (outcome, committed),
            (LegacyRollbackOutcome::CommittedRefreshRequired, true)
                | (LegacyRollbackOutcome::Unknown, false)
        ));
    }
}

#[test]
fn disconnected_mutation_is_unknown() {
    assert!(matches!(
        rollback_outcome(Err(TypedRequestError::Transport {
            method: "thread/rollback".into(),
            source: std::io::Error::new(std::io::ErrorKind::BrokenPipe, "disconnected"),
        })),
        LegacyRollbackOutcome::Unknown
    ));
}

#[test]
fn unclassified_server_failure_does_not_authorize_another_mutation() {
    assert!(matches!(
        rollback_outcome(Err(TypedRequestError::Server {
            method: "thread/rollback".into(),
            source: JSONRPCErrorError {
                code: -32603,
                message: "internal error".into(),
                data: None
            },
        })),
        LegacyRollbackOutcome::Unknown
    ));
}

#[test]
fn validation_rejection_stays_rejected() {
    assert!(matches!(
        rollback_outcome(Err(TypedRequestError::Server {
            method: "thread/rollback".into(),
            source: JSONRPCErrorError {
                code: -32602,
                message: "stale boundary".into(),
                data: None
            },
        })),
        LegacyRollbackOutcome::Rejected(_)
    ));
}
