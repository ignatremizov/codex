use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn observation_preserves_terminal_then_running_without_owning_delivery() {
    let observations = AgentStatusObservations::default();
    let mut subscription = observations.subscribe(AgentStatus::Running);
    let completed = AgentStatus::Completed(Some("done".to_string()));
    observations.publish(&completed);
    observations.publish(&AgentStatus::Running);
    observations.close();
    assert_eq!(subscription.initial_status(), &AgentStatus::Running);
    assert_eq!(subscription.recv().await, Some(completed));
    assert_eq!(subscription.recv().await, Some(AgentStatus::Running));
    assert_eq!(subscription.recv().await, None);
}

#[test]
fn dropped_subscription_unregisters_immediately() {
    let observations = AgentStatusObservations::default();
    let subscription = observations.subscribe(AgentStatus::Running);
    assert_eq!(
        observations
            .subscribers
            .lock()
            .expect("subscribers")
            .senders
            .len(),
        1
    );
    drop(subscription);
    assert!(
        observations
            .subscribers
            .lock()
            .expect("subscribers")
            .senders
            .is_empty()
    );
}

#[tokio::test]
async fn suppression_guard_hides_incidental_shutdown_and_restores_observation() {
    let observations = AgentStatusObservations::default();
    let mut subscription = observations.subscribe(AgentStatus::Running);
    {
        let _guard = observations.suppress();
        observations.publish(&AgentStatus::Shutdown);
        let during_shutdown = observations.subscribe(AgentStatus::Shutdown);
        assert_eq!(during_shutdown.initial_status(), &AgentStatus::Running);
    }
    observations.publish(&AgentStatus::Running);
    observations.close();
    assert_eq!(subscription.recv().await, Some(AgentStatus::Running));
    assert_eq!(subscription.recv().await, None);
}

#[tokio::test]
async fn silent_retirement_closes_without_an_incidental_terminal() {
    for reason in [
        AgentStatusRetirement::ResidencyEviction,
        AgentStatusRetirement::RestoreRollback,
    ] {
        let (session, _turn, _events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let mut subscription = session.subscribe_agent_status_events();
        session.retire_agent_status_observers(reason);
        drop(session);
        assert_eq!(subscription.recv().await, None);
    }
}

#[tokio::test]
async fn committed_disarm_keeps_session_drop_silent() {
    let (session, _turn, _events) = crate::session::tests::make_session_and_context_with_rx().await;
    let mut subscription = session.subscribe_agent_status_events();
    session.disarm_terminal_presentation().commit();
    drop(session);
    assert_eq!(subscription.recv().await, None);
}
