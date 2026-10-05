//! Fault responses at the public request boundary; never issue a second mutation.

use super::*;

pub(super) fn fault(
    behavior: HistoryCapabilities,
    request: &JSONRPCRequest,
    rollback_attempted: &mut bool,
) -> Option<JSONRPCErrorError> {
    if request.method == "thread/rollback" {
        *rollback_attempted = true;
        let (code, data) = match behavior {
            HistoryCapabilities::RollbackRejected => (-32602, None),
            HistoryCapabilities::RollbackUnknown => (
                -32603,
                Some(serde_json::json!({"threadRollbackRefreshRequired": true})),
            ),
            HistoryCapabilities::RollbackCommittedRefreshFails => (
                -32603,
                Some(serde_json::json!({"threadRollbackCommitted": true})),
            ),
            _ => return None,
        };
        return Some(JSONRPCErrorError {
            code,
            data,
            message: "injected rollback outcome".into(),
        });
    }
    if *rollback_attempted
        && request.method == "thread/read"
        && behavior == HistoryCapabilities::RollbackCommittedRefreshFails
    {
        return Some(JSONRPCErrorError {
            code: -32603,
            data: None,
            message: "canonical refresh unavailable".into(),
        });
    }
    None
}
