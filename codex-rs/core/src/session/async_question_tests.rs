use super::super::tests::make_session_and_context;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn concurrent_question_batches_reserve_disjoint_numbers_and_fail_closed_on_exhaustion() {
    let (session, _) = make_session_and_context().await;
    assert!(session.live_thread().is_none());
    let reservations = futures::future::join_all(
        (0..16).map(|_| session.reserve_async_question_refs(/*count*/ 2)),
    )
    .await;
    let mut ranges = reservations
        .into_iter()
        .map(|result| result.expect("reserve an independent batch"))
        .collect::<Vec<_>>();
    ranges.sort_by_key(|range| range.start);
    assert_eq!(
        ranges,
        (0..16)
            .map(|index| (index * 2 + 1)..(index * 2 + 3))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        session.state.lock().await.async_question_high_water,
        Some(32)
    );

    session.state.lock().await.async_question_high_water = Some(u64::MAX - 1);
    assert!(
        session
            .reserve_async_question_refs(/*count*/ 2)
            .await
            .is_err()
    );
    assert_eq!(
        session.state.lock().await.async_question_high_water,
        Some(u64::MAX - 1)
    );
}
