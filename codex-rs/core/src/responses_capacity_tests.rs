use super::retry_capacity_error;
use crate::responses_retry::ExhaustedResponseRetry;
use crate::responses_retry::ResponsesStreamRequest;
use crate::responses_retry::ResponsesStreamRetryState;
use crate::responses_retry::handle_response_stream_error;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::tests::make_session_and_context_with_rx;
use codex_async_utils::OrCancelExt;
use codex_client::RetryOperation;
use codex_http_client::RetryAfter;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InternalSessionSource;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use pretty_assertions::assert_eq;
use std::time::Duration;
use test_case::test_case;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn five_capacity_retries_are_shared_across_streams_and_compaction() -> anyhow::Result<()> {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    let step = StepContext::for_test(turn.clone());
    let mut client = session.services.model_client.new_session();
    tokio::time::pause();
    for attempt in 1..=6 {
        // Every call represents a new sampling/compaction request and an exhausted ordinary budget.
        let mut state = ResponsesStreamRetryState::default();
        let advice = RetryAfter::from_delay(Duration::ZERO).expect("zero delay is valid advice");
        let result = handle_response_stream_error(
            &mut state,
            /*max_retries*/ 0,
            CodexErr::ServerOverloaded.with_retry_after(advice),
            &mut client,
            &session,
            &step,
            if attempt % 2 == 0 {
                ResponsesStreamRequest::RemoteCompactionV2
            } else {
                ResponsesStreamRequest::Sampling
            },
        )
        .await;
        if attempt <= 5 {
            result?;
        } else {
            assert!(matches!(
                result.expect_err("capacity budget exhausted").details(),
                CodexErrorDetails::ServerOverloaded
            ));
        }
        assert_eq!(state.retries, 0);
    }
    let mut retries = Vec::new();
    while let Ok(event) = events.try_recv() {
        match event.msg {
            EventMsg::StreamError(error) => retries.push((
                error.message,
                error.codex_error_info,
                error.retry_at.is_some(),
            )),
            EventMsg::Warning(warning) => panic!("capacity must not switch transport: {warning:?}"),
            _ => {}
        }
    }
    assert_eq!(
        retries,
        (1..=5)
            .map(|attempt| (
                format!("Model at capacity. Retrying... {attempt}/5"),
                Some(CodexErrorInfo::ServerOverloaded),
                true,
            ))
            .collect::<Vec<_>>()
    );

    tokio::time::resume();
    let next_turn = session
        .new_turn_with_default_settings("next-turn".into(), Default::default())
        .await;
    tokio::time::pause();
    retry_capacity_error(
        &session,
        &next_turn,
        CodexErr::ServerOverloaded
            .with_retry_after(RetryAfter::from_delay(Duration::ZERO).expect("zero retry deadline")),
        RetryOperation::Sampling,
    )
    .await?;
    Ok(())
}

#[test_case(SessionSource::Internal(InternalSessionSource::Guardian); "internal")]
#[test_case(
    SessionSource::SubAgent(SubAgentSource::Other(crate::guardian::GUARDIAN_REVIEWER_NAME.to_owned()));
    "legacy_subagent"
)]
#[tokio::test]
async fn internal_review_keeps_its_own_retry_deadline(session_source: SessionSource) {
    let (session, mut turn) = make_session_and_context().await;
    turn.session_source = session_source;
    let deadline = RetryAfter::from_delay(Duration::from_secs(/*secs*/ 12)).expect("retry advice");
    let result = retry_capacity_error(
        &session,
        &turn,
        CodexErr::ServerOverloaded.with_retry_after(deadline),
        RetryOperation::Sampling,
    )
    .await;
    assert!(matches!(
        result.unwrap_err().details(),
        CodexErrorDetails::ServerOverloaded
    ));
    let advice = session
        .services
        .thread_extension_data
        .get::<ExhaustedResponseRetry>()
        .expect("review owner receives retry advice");
    assert_eq!(
        (advice.turn_id.as_str(), advice.retry_at),
        (turn.sub_id.as_str(), Some(deadline.deadline()))
    );
}

#[tokio::test]
async fn capacity_wait_honors_the_original_deadline_and_cancellation() -> anyhow::Result<()> {
    let (session, turn) = make_session_and_context().await;
    tokio::time::pause();
    let advice = RetryAfter::from_delay(Duration::from_secs(/*secs*/ 20)).expect("retry advice");
    tokio::time::advance(Duration::from_secs(/*secs*/ 7)).await;
    let retry = retry_capacity_error(
        &session,
        &turn,
        CodexErr::ServerOverloaded.with_retry_after(advice),
        RetryOperation::Sampling,
    );
    tokio::pin!(retry);
    assert!(futures::poll!(&mut retry).is_pending());
    tokio::time::advance(Duration::from_secs(/*secs*/ 12)).await;
    assert!(futures::poll!(&mut retry).is_pending());
    retry.await?;
    assert!(Instant::now() <= advice.deadline() + Duration::from_millis(/*millis*/ 1));

    let cancellation = CancellationToken::new();
    let retry = retry_capacity_error(
        &session,
        &turn,
        CodexErr::ServerOverloaded,
        RetryOperation::Sampling,
    )
    .or_cancel(&cancellation);
    tokio::pin!(retry);
    assert!(futures::poll!(&mut retry).is_pending());
    cancellation.cancel();
    assert!(retry.await.is_err());
    Ok(())
}
