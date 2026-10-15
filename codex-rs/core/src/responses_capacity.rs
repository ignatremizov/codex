//! Patient, same-turn recovery for structured model-capacity failures.
//!
//! The turn owns the budget, including compaction and later sampling steps. Transport fallback,
//! new streams, and partial model progress must not silently replenish it.

use crate::responses_retry::ExhaustedResponseRetry;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_client::RetryOperation;
use codex_http_client::RetryAfter;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::StreamErrorEvent;
use rand::Rng;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::Instant;
use tracing::warn;

const MAX_CAPACITY_RETRIES: u64 = 5;

#[derive(Default)]
struct CapacityRetries(AtomicU64);

/// Called only for `ServerOverloaded`, before ordinary stream retries or transport fallback.
/// The caller owns cancellation of both this wait and the subsequent model request.
pub(crate) async fn retry_capacity_error(
    sess: &Session,
    turn: &TurnContext,
    err: CodexErr,
    operation: RetryOperation,
) -> Result<(), CodexErr> {
    // Internal reviewers already have a deadline-bound retry owner. Do not nest a second policy.
    if turn.session_source.is_internal()
        || crate::guardian::is_basic_session_source(&turn.session_source)
    {
        sess.services
            .thread_extension_data
            .insert(ExhaustedResponseRetry {
                turn_id: turn.sub_id.clone(),
                retry_at: err.retry_after().map(RetryAfter::deadline),
            });
        return Err(err);
    }
    let budget = turn.extension_data.get_or_init(CapacityRetries::default);
    let attempt = match budget
        .0
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |attempt| {
            (attempt < MAX_CAPACITY_RETRIES).then_some(attempt + 1)
        }) {
        Ok(previous) => previous + 1,
        Err(_) => return Err(err),
    };

    let now = Instant::now();
    // Reuse the reconnect cadence, with positive jitter to spread concurrent agent retries.
    let base_seconds = (5 << (attempt - 1)).min(60);
    let delay = Duration::from_millis(
        base_seconds * 1_000 + rand::rng().random_range(0..=base_seconds * 100),
    );
    // Server advice is an absolute deadline: never restart it after notification delivery.
    let retry_at = err
        .retry_after()
        .map(RetryAfter::deadline)
        .unwrap_or(now + delay);
    let delay = retry_at.saturating_duration_since(now);
    let wall_deadline = chrono::Utc::now()
        .timestamp_millis()
        .saturating_add(i64::try_from(delay.as_millis()).unwrap_or(i64::MAX));
    // Round up so the display never promises a retry before the monotonic timer expires.
    let display_deadline = wall_deadline.saturating_add(999) / 1_000;
    warn!(
        turn_id = %turn.sub_id,
        attempt,
        ?delay,
        "model at capacity; retrying the current turn"
    );
    codex_client::record_retry!(attempt, delay, operation);
    sess.send_event(
        turn,
        EventMsg::StreamError(StreamErrorEvent {
            message: format!("Model at capacity. Retrying... {attempt}/{MAX_CAPACITY_RETRIES}"),
            codex_error_info: Some(CodexErrorInfo::ServerOverloaded),
            additional_details: Some(err.to_string()),
            retry_at: Some(display_deadline),
        }),
    )
    .await;
    tokio::time::sleep_until(retry_at).await;
    Ok(())
}

#[cfg(test)]
#[path = "responses_capacity_tests.rs"]
mod tests;
